use super::Config;
use drive3;
use failure::{err_msg, Error};
use hyper;
use hyper::client::Response;
use hyper_rustls;
use lru_time_cache::LruCache;
use mime_sniffer::MimeTypeSniffer;
use oauth2;
use serde_json;
use std::cmp;
use std::collections::HashMap;
use std::io;
use std::io::{Read, Seek, SeekFrom};

const PAGE_SIZE: i32 = 1000;
const CLIENT_SECRET: &str = "{\"installed\":{\"client_id\":\"726003905312-e2mq9mesjc5llclmvc04ef1k7qopv9tu.apps.googleusercontent.com\",\"project_id\":\"weighty-triode-199418\",\"auth_uri\":\"https://accounts.google.com/o/oauth2/auth\",\"token_uri\":\"https://accounts.google.com/o/oauth2/token\",\"auth_provider_x509_cert_url\":\"https://www.googleapis.com/oauth2/v1/certs\",\"client_secret\":\"hp83n1Rzz8UpxgCnqvX15qC2\",\"redirect_uris\":[\"urn:ietf:wg:oauth:2.0:oob\",\"http://localhost\"]}}";

type DriveId = String;

type GCClient = hyper::Client;
type GCAuthenticator = oauth2::Authenticator<
    oauth2::DefaultAuthenticatorDelegate,
    oauth2::DiskTokenStorage,
    hyper::Client,
>;
type GCDrive = drive3::Drive<GCClient, GCAuthenticator>;

/// Provides a simple high-level interface for interacting with the Google Drive API.
pub struct DriveFacade {
    /// The `drive3::Drive` hub used for interacting with the API.
    pub hub: GCDrive,

    /// A buffer used for temporarily caching read blocks. Storing this inside the struct makes it possible to return a reference to the data without the danger of the data outliving the struct.
    buff: Vec<u8>,

    /// Maps Drive IDs to a list of pending write operations that must be applied on them.
    pending_writes: HashMap<DriveId, Vec<PendingWrite>>,

    /// The LRU cache used for storing the file contents for any given Drive ID.
    cache: LruCache<DriveId, Vec<u8>>,

    /// Keeps track of the page token used for receiving changes from the `changes.list` API endpoint.
    changes_token: Option<String>,

    /// The root id is only stored once, effectively caching the root id.
    root_id: Option<String>,
}

/// Represents a write operation that has been performed from the user's point of view but has not
/// yet been applied to the local or remote file.
#[derive(Debug)]
struct PendingWrite {
    id: DriveId,
    offset: usize,
    data: Vec<u8>,
}

lazy_static! {
    static ref MIME_TYPES: HashMap<&'static str, &'static str> = hashmap!{
        "application/vnd.google-apps.document" => "application/vnd.oasis.opendocument.text",
        "application/vnd.google-apps.presentation" => "application/vnd.oasis.opendocument.presentation",
        "application/vnd.google-apps.spreadsheet" => "application/vnd.oasis.opendocument.spreadsheet",
    };
}

impl DriveFacade {
    pub fn new(config: &Config) -> Self {
        debug!("DriveFacade::new()");

        let ttl = config.cache_max_seconds();
        let max_count = config.cache_max_items() as usize;

        DriveFacade {
            hub: DriveFacade::create_drive(&config).unwrap(),
            buff: Vec::new(),
            pending_writes: HashMap::new(),
            cache: LruCache::<String, Vec<u8>>::with_expiry_duration_and_capacity(ttl, max_count),
            root_id: None,
            changes_token: None,
        }
    }

    fn create_drive_auth(config: &Config) -> Result<GCAuthenticator, Error> {
        let secret: oauth2::ConsoleApplicationSecret = serde_json::from_str(CLIENT_SECRET)?;
        let secret = secret
            .installed
            .ok_or(err_msg("ConsoleApplicationSecret.installed is None"))?;

        let auth = oauth2::Authenticator::new(
            &secret,
            oauth2::DefaultAuthenticatorDelegate,
            hyper::Client::with_connector(hyper::net::HttpsConnector::new(
                hyper_rustls::TlsClient::new(),
            )),
            oauth2::DiskTokenStorage::new(&config.token_file().to_str().unwrap().to_string())
                .unwrap(),
            Some(if config.authorize_using_code() {
                oauth2::FlowType::InstalledInteractive
            } else {
                oauth2::FlowType::InstalledRedirect(8081)
            }),
        );

        Ok(auth)
    }

    /// Creates a drive hub.
    fn create_drive(config: &Config) -> Result<GCDrive, Error> {
        let auth = Self::create_drive_auth(config)?;
        Ok(drive3::Drive::new(
            hyper::Client::with_connector(hyper::net::HttpsConnector::new(
                hyper_rustls::TlsClient::new(),
            )),
            auth,
        ))
    }

    /// Will still detect a file even if it is in Trash.
    fn contains(&self, id: &DriveId) -> Result<bool, Error> {
        let response = self.hub
            .files()
            .get(&id)
            .add_scope(drive3::Scope::Full)
            .doit();

        match response {
            Ok((_, file)) => Ok(file.id == Some(id.to_string())),
            Err(e) => Err(err_msg(format!("{:#?}", e))),
        }
    }

    #[allow(dead_code)]
    fn get_file_size(&self, drive_id: &str, mime_type: Option<String>) -> u64 {
        self.get_file_content(drive_id, mime_type).unwrap().len() as u64
    }

    fn get_file_metadata(&self, id: &str) -> Result<drive3::File, Error> {
        self.hub
            .files()
            .get(id)
            .param("fields", "id,name,parents,mimeType")
            .add_scope(drive3::Scope::Full)
            .doit()
            .map(|(_response, file)| file)
            .map_err(|e| err_msg(format!("{:#?}", e)))
    }

    /// Retrieves the content of a Drive file. If `mime_type` is specified, this method will
    /// attempt to export the file in some appropriate format rather than just download it as is.
    /// This is the only way of retrieving Docs, Sheets and Slides.
    fn get_file_content(
        &self,
        drive_id: &str,
        mime_type: Option<String>,
    ) -> Result<Vec<u8>, Error> {
        let export_type: Option<&'static str> = mime_type
            .and_then(|ref t| MIME_TYPES.get::<str>(&t))
            .cloned();

        let mut response = match export_type {
            Some(t) => {
                let mut response = self.hub
                    .files()
                    .export(drive_id, &t)
                    .add_scope(drive3::Scope::Full)
                    .doit()
                    .map_err(|e| err_msg(format!("{:#?}", e)))?;

                debug!("response: {:?}", &response);
                response
            }
            None => {
                let (mut response, _empty_file) = self.hub
                    .files()
                    .get(&drive_id)
                    .supports_team_drives(false)
                    .param("alt", "media")
                    .add_scope(drive3::Scope::Full)
                    .doit()
                    .map_err(|e| err_msg(format!("{:#?}", e)))?;
                response
            }
        };

        let mut content: Vec<u8> = Vec::new();
        let _result = response.read_to_end(&mut content);

        Ok(content)
    }

    /// Applies all pending writes accumulated so far on a data buffer. The pending writes are then
    /// cleared.
    fn apply_pending_writes_on_data(&mut self, id: DriveId, data: &mut Vec<u8>) {
        self.pending_writes
            .entry(id.clone())
            .or_insert(Vec::new())
            .iter()
            .filter(|write| write.id == id)
            .for_each(|pending_write| {
                debug!(
                    "Applying pending write with offset {} on {}",
                    &pending_write.offset, &pending_write.id
                );
                let required_size = pending_write.offset + pending_write.data.len();

                data.resize(required_size, 0);
                data[pending_write.offset..].copy_from_slice(&pending_write.data[..]);
            });

        self.pending_writes.remove(&id);
    }

    /// Returns the Drive ID of the root "My Drive" directory
    pub fn root_id(&mut self) -> Result<&String, Error> {
        if self.root_id.is_some() {
            return Ok(self.root_id.as_ref().unwrap());
        }

        let parent = self.hub
            .files()
            .list()
            .param("fields", "files(parents)")
            .spaces("drive")
            .corpora("user")
            .page_size(1)
            .q("'root' in parents")
            .add_scope(drive3::Scope::Full)
            .doit()
            .map_err(|e| err_msg(format!("{:#?}", e)))?
            .1
            .files
            .ok_or(err_msg("No files received"))?
            .into_iter()
            .take(1)
            .next()
            .ok_or(err_msg(
                "No files on drive. Can't deduce drive id for 'My Drive'",
            ))?
            .parents
            .ok_or(err_msg(
                "Probed file has no parents. Can't deduce drive id for 'My Drive'",
            ))?
            .into_iter()
            .take(1)
            .next()
            .ok_or(err_msg(
                "No files on drive. Can't deduce drive id for 'My Drive'",
            ))?;

        self.root_id = Some(parent);
        Ok(self.root_id.as_ref().unwrap())
    }

    /// Returns the start page token for the `changes.list` API endpoint.
    fn get_start_page_token(&mut self) -> Result<String, Error> {
        self.hub
            .changes()
            .get_start_page_token()
            .add_scope(drive3::Scope::Full)
            .doit()
            .map_err(|e| err_msg(format!("{:#?}", e)))
            .map(|result| {
                result.1.start_page_token.unwrap_or(
                    err_msg(
                        "Received OK response from drive but there is no startPageToken included.",
                    ).to_string(),
                )
            })
    }

    /// Returns the current token for the `changes.list` API endpoint, or the start page token if
    /// absent.
    fn changes_token(&mut self) -> Result<&String, Error> {
        if self.changes_token.is_none() {
            self.changes_token = Some(self.get_start_page_token()?);
        }

        Ok(self.changes_token.as_ref().unwrap())
    }

    /// Returns a list of all changes reported by Drive which are more recent than the changes
    /// token indicates.
    pub fn get_all_changes(&mut self) -> Result<Vec<drive3::Change>, Error> {
        let mut all_changes = Vec::new();

        loop {
            let token = self.changes_token()?.clone();
            let (_response, changelist) = self.hub
                .changes()
                .list(&token)
                .param("fields", "kind,newStartPageToken,changes(kind,type,time,removed,fileId,file(name,id,size,mimeType,owners,parents,trashed,modifiedTime,createdTime,viewedByMeTime))")
                .spaces("drive")
                .restrict_to_my_drive(true)
                // Whether to include changes indicating that items have been removed from the list of changes, for example by deletion or loss of access. (Default: true)
                .include_removed(false) // ^wtf?
                .supports_team_drives(false)
                .include_team_drive_items(false)
                .page_size(PAGE_SIZE)
                .add_scope(drive3::Scope::Full)
                .doit()
                .map_err(|e| err_msg(format!("{:#?}", e)))?;

            match changelist.changes {
                Some(changes) => all_changes.extend(changes),
                _ => warn!("Changelist does not contain any changes!"),
            };

            self.changes_token = changelist.next_page_token;
            if self.changes_token.is_none() {
                self.changes_token = changelist.new_start_page_token;
                break;
            }
        }

        Ok(all_changes)
    }

    /// Returns a list of all files from Drive. If the `parents` list is provided, only files which are children of any one of the list's elements are returned. If `trashed` is provided, only files which are trashed/not trashed are returned. The two filters can be used together.
    pub fn get_all_files(
        &mut self,
        parents: Option<Vec<DriveId>>,
        trashed: Option<bool>,
    ) -> Result<Vec<drive3::File>, Error> {
        let mut all_files = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut request = self.hub.files()
                .list()
                .param("fields", "nextPageToken,files(name,id,size,mimeType,owners,parents,trashed,modifiedTime,createdTime,viewedByMeTime)")
                .spaces("drive") // TODO: maybe add photos as well
                .corpora("user")
                .page_size(PAGE_SIZE)
                .add_scope(drive3::Scope::Full);

            if let Some(token) = page_token {
                request = request.page_token(&token);
            };

            let mut query_chain: Vec<String> = Vec::new();
            if let Some(ref p) = parents {
                let q = p.into_iter()
                    .map(|id| format!("'{}' in parents", id))
                    .collect::<Vec<_>>()
                    .join(" or ");

                query_chain.push(format!("({})", q));
            }
            if let Some(trash) = trashed {
                query_chain.push(format!("trashed = {}", trash));
            }

            let query = query_chain.join(" and ");
            let (_, filelist) = request
                .q(&query)
                .doit()
                .map_err(|e| err_msg(format!("{:#?}", e)))?;

            match filelist.files {
                Some(files) => all_files.extend(files),
                _ => warn!("Filelist does not contain any files!"),
            };

            page_token = filelist.next_page_token;
            if page_token.is_none() {
                break;
            }
        }
        return Ok(all_files);
    }

    pub fn read(
        &mut self,
        drive_id: &str,
        mime_type: Option<String>,
        offset: usize,
        size: usize,
    ) -> Option<&[u8]> {
        if self.cache.contains_key(drive_id) {
            let data = self.cache.get(drive_id).unwrap();
            self.buff =
                data[cmp::min(data.len(), offset)..cmp::min(data.len(), offset + size)].to_vec();
            return Some(&self.buff);
        }

        match self.get_file_content(&drive_id, mime_type) {
            Ok(data) => {
                self.buff = data[cmp::min(data.len(), offset)..cmp::min(data.len(), offset + size)]
                    .to_vec();
                self.cache.insert(drive_id.to_string(), data.to_vec());
                Some(&self.buff)
            }
            Err(e) => {
                error!("Got error: {:?}", e);
                None
            }
        }
    }

    pub fn create(&mut self, drive_file: &drive3::File) -> Result<DriveId, Error> {
        let dummy_file = DummyFile::new(&[]);
        self.hub
            .files()
            .create(drive_file.clone())
            .use_content_as_indexable_text(true)
            .supports_team_drives(false)
            .ignore_default_visibility(true)
            .upload(dummy_file, "application/octet-stream".parse().unwrap())
            .map_err(|e| err_msg(format!("{:#?}", e)))
            .map(|(_, file)| {
                file.id.unwrap_or(
                    err_msg("Received file from drive but it has no drive id.").to_string(),
                )
            })
    }

    pub fn write(&mut self, id: DriveId, offset: usize, data: &[u8]) {
        let pending_write = PendingWrite {
            id: id.clone(),
            offset,
            data: data.to_vec(),
        };

        self.pending_writes
            .entry(id.clone())
            .or_insert(Vec::with_capacity(3000))
            .push(pending_write);
    }

    pub fn delete_permanently(&mut self, id: &DriveId) -> Result<bool, Error> {
        self.hub
            .files()
            .delete(&id)
            .supports_team_drives(false)
            .add_scope(drive3::Scope::Full)
            .doit()
            .map(|response| response.status.is_success())
            .map_err(|e| err_msg(format!("{:#?}", e)))
    }

    pub fn move_to(
        &mut self,
        id: &DriveId,
        parent: &DriveId,
        new_name: &str,
    ) -> Result<(Response, drive3::File), Error> {
        let current_parents = self.get_file_metadata(id)?
            .parents
            .unwrap_or(vec![String::from("root")])
            .join(",");

        let mut file = drive3::File::default();
        file.name = Some(new_name.to_string());
        self.hub
            .files()
            .update(file, id)
            .remove_parents(&current_parents)
            .add_parents(parent)
            .add_scope(drive3::Scope::Full)
            .doit_without_upload()
            .map_err(|e| err_msg(format!("DriveFacade::move_to() {}", e)))
    }

    pub fn move_to_trash(&mut self, id: DriveId) -> Result<(), Error> {
        let mut f = drive3::File::default();
        f.trashed = Some(true);

        self.hub
            .files()
            .update(f, &id)
            .add_scope(drive3::Scope::Full)
            .doit_without_upload()
            .map(|_| ())
            .map_err(|e| err_msg(format!("DriveFacade::move_to() {}", e)))
    }

    pub fn flush(&mut self, id: &DriveId) -> Result<(), Error> {
        if !self.pending_writes.contains_key(id) {
            debug!("flush({}): no pending writes", id);
            return Ok(());
        }
        self.cache.remove(id);

        if let Ok(false) = self.contains(id) {
            return Err(err_msg(format!(
                "flush({}): file doesn't exist on drive!",
                id
            )));
        }

        let mut file_data = self.get_file_content(&id, None).unwrap_or_default();
        self.apply_pending_writes_on_data(id.clone(), &mut file_data);
        self.update_file_content(id.clone(), &file_data)?;

        Ok(())
    }

    /// Updates the content of a file on Drive. The MIME type is guessed appropriately based on the
    /// content.
    fn update_file_content(
        &mut self,
        id: DriveId,
        data: &[u8],
    ) -> Result<(Response, drive3::File), Error> {
        let mime_guess = data.sniff_mime_type().unwrap_or("application/octet-stream");
        debug!(
            "Updating file content for {}. Mime type guess based on content: {}",
            &id, &mime_guess
        );

        let mut file = drive3::File::default();
        file.mime_type = Some(mime_guess.to_string());

        self.hub
            .files()
            .update(file, &id)
            .add_scope(drive3::Scope::Full)
            .upload_resumable(DummyFile::new(data), mime_guess.parse().unwrap())
            .map_err(|e| err_msg(format!("{:#?}", e)))
    }

    /// Returns the size and capacity of the Drive account. In some cases, the limit can be absent.
    pub fn size_and_capacity(&mut self) -> Result<(u64, Option<u64>), Error> {
        let (_response, about) = self.hub
            .about()
            .get()
            .param("fields", "storageQuota")
            .add_scope(drive3::Scope::Full)
            .doit()
            .map_err(|e| err_msg(format!("{:#?}", e)))?;

        let storage_quota = about
            .storage_quota
            .ok_or(err_msg("size_and_capacity(): no storage quota in response"))?;

        let usage = storage_quota.usage.unwrap().parse::<u64>().unwrap();
        let limit = storage_quota.limit.map(|s| s.parse::<u64>().unwrap());

        Ok((usage, limit))
    }
}

/// A virtual (in-memory) file which implements the Read + Seek traits. Can be constructed from a
/// slice of bytes. Useful for uploading some file content to Drive without actually storing the
/// file locally on disk.
struct DummyFile {
    cursor: u64,
    data: Vec<u8>,
}

impl DummyFile {
    fn new(data: &[u8]) -> DummyFile {
        DummyFile {
            cursor: 0,
            data: Vec::from(data),
        }
    }
}

impl Seek for DummyFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let position: i64 = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.data.len() as i64 - offset,
            SeekFrom::Current(offset) => self.cursor as i64 + offset,
        };

        if position < 0 {
            Err(io::Error::from(io::ErrorKind::InvalidInput))
        } else {
            self.cursor = position as u64;
            Ok(self.cursor)
        }
    }
}

impl Read for DummyFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.data.len() - self.cursor as usize;
        let copied = cmp::min(buf.len(), remaining);

        if copied > 0 {
            buf[..]
                .copy_from_slice(&self.data[self.cursor as usize..self.cursor as usize + copied]);
        }

        self.cursor += copied as u64;
        Ok(copied)
    }
}
