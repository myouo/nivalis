use mail_parser::{HeaderName, Message, MessageParser, MimeHeaders, PartType};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const IO_BUFFER_BYTES: usize = 64 * 1024;
const MAX_SUBJECT_BYTES: usize = 998;
const MAX_ADDRESS_BYTES: usize = 320;
const MAX_CONTENT_ID_BYTES: usize = 998;
const MAX_MEDIA_TYPE_BYTES: usize = 255;
const MAX_DISPOSITION_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentLimits {
    pub(crate) raw_message_bytes: usize,
    pub(crate) parser_sections: usize,
    pub(crate) parser_allocation_bytes: usize,
    pub(crate) header_block_bytes: usize,
    pub(crate) total_header_bytes: usize,
    pub(crate) header_fields: usize,
    pub(crate) mime_parts: usize,
    pub(crate) mime_depth: usize,
    pub(crate) nested_messages: usize,
    pub(crate) decoded_part_bytes: usize,
    pub(crate) decoded_total_bytes: usize,
    pub(crate) attachments: usize,
    pub(crate) stored_body_bytes: usize,
    pub(crate) preview_bytes: usize,
    pub(crate) reader_excerpt_bytes: usize,
    pub(crate) quoted_history_bytes: usize,
    pub(crate) display_file_name_bytes: usize,
}

impl Default for ContentLimits {
    fn default() -> Self {
        Self {
            raw_message_bytes: 8 * 1024 * 1024,
            parser_sections: 512,
            parser_allocation_bytes: 32 * 1024 * 1024,
            header_block_bytes: 64 * 1024,
            total_header_bytes: 256 * 1024,
            header_fields: 512,
            mime_parts: 128,
            mime_depth: 8,
            nested_messages: 8,
            decoded_part_bytes: 8 * 1024 * 1024,
            decoded_total_bytes: 16 * 1024 * 1024,
            attachments: 32,
            stored_body_bytes: 4 * 1024 * 1024,
            preview_bytes: 2 * 1024,
            reader_excerpt_bytes: 64 * 1024,
            quoted_history_bytes: 16 * 1024,
            display_file_name_bytes: 255,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MimeResource {
    RawMessageBytes,
    ParserSections,
    ParserAllocationBytes,
    HeaderBlockBytes,
    TotalHeaderBytes,
    HeaderFields,
    MimeParts,
    MimeDepth,
    NestedMessages,
    DecodedPartBytes,
    DecodedTotalBytes,
    Attachments,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MalformedMime {
    NoHeaders,
    InvalidHeaders,
    InvalidPartIndex,
    PartCycle,
    MultipleParents,
    UnreachablePart,
    InvalidOffsets,
    InvalidBodyProjection,
    MultipartAttachment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MimeError {
    LimitExceeded {
        resource: MimeResource,
        observed: usize,
        maximum: usize,
    },
    Malformed(MalformedMime),
}

impl fmt::Display for MimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                resource,
                observed,
                maximum,
            } => write!(
                formatter,
                "MIME {resource:?} limit exceeded ({observed} > {maximum})"
            ),
            Self::Malformed(reason) => write!(formatter, "malformed MIME structure: {reason:?}"),
        }
    }
}

impl std::error::Error for MimeError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StorageOperation {
    ValidateRoot,
    CreateDirectory,
    CreateTemporary,
    ReadInput,
    WriteTemporary,
    SyncTemporary,
    Publish,
    RemoveTemporary,
    OpenPublished,
    RemovePublished,
    SyncDirectory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoveOutcome {
    Removed,
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StorageError {
    pub(crate) operation: StorageOperation,
    pub(crate) kind: io::ErrorKind,
}

impl StorageError {
    fn new(operation: StorageOperation, error: &io::Error) -> Self {
        Self {
            operation,
            kind: error.kind(),
        }
    }

    fn invalid(operation: StorageOperation) -> Self {
        Self {
            operation,
            kind: io::ErrorKind::InvalidInput,
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "content storage {:?} failed ({:?})",
            self.operation, self.kind
        )
    }
}

impl std::error::Error for StorageError {}

#[derive(Debug)]
pub(crate) enum ContentError {
    Mime(MimeError),
    Storage(StorageError),
}

impl fmt::Display for ContentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mime(error) => error.fmt(formatter),
            Self::Storage(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ContentError {}

impl From<MimeError> for ContentError {
    fn from(error: MimeError) -> Self {
        Self::Mime(error)
    }
}

impl From<StorageError> for ContentError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileKey(Box<str>);

impl FileKey {
    pub(crate) fn parse(value: &str) -> Result<Self, StorageError> {
        if value.contains('\\') || value.contains('\0') {
            return Err(StorageError::invalid(StorageOperation::ValidateRoot));
        }
        let Some((kind, file_name)) = value.split_once('/') else {
            return Err(StorageError::invalid(StorageOperation::ValidateRoot));
        };
        if file_name.contains('/') {
            return Err(StorageError::invalid(StorageOperation::ValidateRoot));
        }
        let extension = match kind {
            "body" => ".txt",
            "attachment" => ".bin",
            _ => return Err(StorageError::invalid(StorageOperation::ValidateRoot)),
        };
        let Some(token) = file_name.strip_suffix(extension) else {
            return Err(StorageError::invalid(StorageOperation::ValidateRoot));
        };
        if token.len() != 32
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(StorageError::invalid(StorageOperation::ValidateRoot));
        }
        Ok(Self(value.into()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn kind(&self) -> FileKind {
        if self.0.starts_with("body/") {
            FileKind::Body
        } else {
            FileKind::Attachment
        }
    }

    fn file_name(&self) -> &str {
        self.0
            .split_once('/')
            .map(|(_, file_name)| file_name)
            .expect("validated file key has one separator")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContentRecord {
    pub(crate) subject: Box<str>,
    pub(crate) sender_name: Box<str>,
    pub(crate) sender_address: Box<str>,
    pub(crate) received_at_ms: Option<i64>,
    pub(crate) preview: Box<str>,
    pub(crate) reader_excerpt: Box<str>,
    pub(crate) body_truncated: bool,
    pub(crate) body_byte_count: u64,
    pub(crate) body_file_key: Option<FileKey>,
    pub(crate) attachments: Box<[AttachmentRecord]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttachmentRecord {
    pub(crate) ordinal: u16,
    pub(crate) file_name: Box<str>,
    pub(crate) media_type: Box<str>,
    pub(crate) content_id: Option<Box<str>>,
    pub(crate) disposition: Box<str>,
    pub(crate) byte_count: u64,
    pub(crate) file_key: FileKey,
}

#[derive(Debug)]
struct ContentMetadata {
    subject: Box<str>,
    sender_name: Box<str>,
    sender_address: Box<str>,
    received_at_ms: Option<i64>,
    preview: Box<str>,
    reader_excerpt: Box<str>,
    body_truncated: bool,
    body_byte_count: u64,
}

#[derive(Debug)]
pub(crate) struct PreparedContent {
    metadata: ContentMetadata,
    body: Option<StagedFile>,
    attachments: Box<[PreparedAttachment]>,
}

#[derive(Debug)]
struct PreparedAttachment {
    metadata: AttachmentMetadata,
    file: StagedFile,
}

#[derive(Debug)]
struct AttachmentMetadata {
    ordinal: u16,
    file_name: Box<str>,
    media_type: Box<str>,
    content_id: Option<Box<str>>,
    disposition: Box<str>,
    byte_count: u64,
}

impl PreparedContent {
    pub(crate) fn record(&self) -> ContentRecord {
        make_record(
            &self.metadata,
            self.body.as_ref().map(|file| &file.key),
            self.attachments
                .iter()
                .map(|attachment| (&attachment.metadata, &attachment.file.key)),
        )
    }

    pub(crate) fn publish(self) -> Result<PublishedContent, StorageError> {
        let body = self.body.map(StagedFile::publish).transpose()?;
        let mut attachments = Vec::with_capacity(self.attachments.len());
        for attachment in self.attachments.into_vec() {
            attachments.push(PublishedAttachment {
                metadata: attachment.metadata,
                file: attachment.file.publish()?,
            });
        }
        Ok(PublishedContent {
            metadata: self.metadata,
            body,
            attachments: attachments.into_boxed_slice(),
        })
    }
}

#[derive(Debug)]
pub(crate) struct PublishedContent {
    metadata: ContentMetadata,
    body: Option<PublishedFile>,
    attachments: Box<[PublishedAttachment]>,
}

#[derive(Debug)]
struct PublishedAttachment {
    metadata: AttachmentMetadata,
    file: PublishedFile,
}

impl PublishedContent {
    pub(crate) fn record(&self) -> ContentRecord {
        make_record(
            &self.metadata,
            self.body.as_ref().map(|file| &file.key),
            self.attachments
                .iter()
                .map(|attachment| (&attachment.metadata, &attachment.file.key)),
        )
    }

    pub(crate) fn retain_files(&mut self) {
        if let Some(body) = &mut self.body {
            body.retained = true;
        }
        for attachment in &mut self.attachments {
            attachment.file.retained = true;
        }
    }
}

fn make_record<'a>(
    metadata: &ContentMetadata,
    body_file_key: Option<&FileKey>,
    attachments: impl Iterator<Item = (&'a AttachmentMetadata, &'a FileKey)>,
) -> ContentRecord {
    ContentRecord {
        subject: metadata.subject.clone(),
        sender_name: metadata.sender_name.clone(),
        sender_address: metadata.sender_address.clone(),
        received_at_ms: metadata.received_at_ms,
        preview: metadata.preview.clone(),
        reader_excerpt: metadata.reader_excerpt.clone(),
        body_truncated: metadata.body_truncated,
        body_byte_count: metadata.body_byte_count,
        body_file_key: body_file_key.cloned(),
        attachments: attachments
            .map(|(metadata, key)| AttachmentRecord {
                ordinal: metadata.ordinal,
                file_name: metadata.file_name.clone(),
                media_type: metadata.media_type.clone(),
                content_id: metadata.content_id.clone(),
                disposition: metadata.disposition.clone(),
                byte_count: metadata.byte_count,
                file_key: key.clone(),
            })
            .collect(),
    }
}

#[derive(Debug)]
pub(crate) struct ContentStaging {
    root: PathBuf,
    body: PathBuf,
    attachment: PathBuf,
}

impl ContentStaging {
    pub(crate) fn open(root: PathBuf) -> Result<Self, StorageError> {
        #[cfg(not(unix))]
        {
            let _ = root;
            return Err(StorageError {
                operation: StorageOperation::ValidateRoot,
                kind: io::ErrorKind::Unsupported,
            });
        }
        #[cfg(unix)]
        {
            if !root.is_absolute() {
                return Err(StorageError::invalid(StorageOperation::ValidateRoot));
            }
            ensure_private_directory(&root)?;
            let body = root.join("body");
            let attachment = root.join("attachment");
            for directory in [&body, &attachment] {
                ensure_private_directory(directory)?;
            }
            sync_directory(&root)?;
            Ok(Self {
                root,
                body,
                attachment,
            })
        }
    }

    fn resolve(&self, key: &FileKey) -> Result<PathBuf, StorageError> {
        self.published_location(key, StorageOperation::ValidateRoot)
            .map(|(_, path)| path)
    }

    fn published_location(
        &self,
        key: &FileKey,
        operation: StorageOperation,
    ) -> Result<(&Path, PathBuf), StorageError> {
        let key = FileKey::parse(key.as_str()).map_err(|_| StorageError::invalid(operation))?;
        let directory = match key.kind() {
            FileKind::Body => self.body.as_path(),
            FileKind::Attachment => self.attachment.as_path(),
        };
        Ok((directory, directory.join(key.file_name())))
    }

    pub(crate) fn open_file(&self, key: &FileKey) -> Result<File, StorageError> {
        let operation = StorageOperation::OpenPublished;
        let (directory, path) = self.published_location(key, operation)?;
        let directory_before = validate_published_directory(directory, operation)?;
        let path_before =
            fs::symlink_metadata(&path).map_err(|error| StorageError::new(operation, &error))?;
        if path_before.file_type().is_symlink() || !path_before.is_file() {
            return Err(StorageError::invalid(operation));
        }

        // TODO(M7): use directory-handle openat/O_NOFOLLOW to close the path race.
        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(|error| StorageError::new(operation, &error))?;
        let opened = file
            .metadata()
            .map_err(|error| StorageError::new(operation, &error))?;
        let path_after =
            fs::symlink_metadata(&path).map_err(|error| StorageError::new(operation, &error))?;
        let directory_after = validate_published_directory(directory, operation)?;
        if !opened.is_file()
            || path_after.file_type().is_symlink()
            || !path_after.is_file()
            || !same_file_identity(&path_before, &opened)
            || !same_file_identity(&opened, &path_after)
            || !same_file_identity(&directory_before, &directory_after)
        {
            return Err(StorageError::invalid(operation));
        }
        Ok(file)
    }

    pub(crate) fn remove_published_file(
        &self,
        key: &FileKey,
    ) -> Result<RemoveOutcome, StorageError> {
        let operation = StorageOperation::RemovePublished;
        let (directory, path) = self.published_location(key, operation)?;
        let directory_before = validate_published_directory(directory, operation)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(RemoveOutcome::Missing);
            }
            Err(error) => return Err(StorageError::new(operation, &error)),
        };
        if !metadata.is_file() && !metadata.file_type().is_symlink() {
            return Err(StorageError::invalid(operation));
        }

        // TODO(M7): use directory-handle unlinkat to close the parent-swap race.
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(RemoveOutcome::Missing);
            }
            Err(error) => return Err(StorageError::new(operation, &error)),
        }
        sync_parent_directory(&path)?;
        let directory_after = validate_published_directory(directory, operation)?;
        if !same_file_identity(&directory_before, &directory_after) {
            return Err(StorageError::invalid(operation));
        }
        Ok(RemoveOutcome::Removed)
    }

    pub(crate) fn stage_reader(
        &self,
        kind: FileKind,
        mut reader: impl Read,
        maximum: usize,
    ) -> Result<StagedFile, StorageError> {
        for _ in 0..8 {
            let token = unique_token();
            let (directory, extension) = match kind {
                FileKind::Body => (&self.body, "txt"),
                FileKind::Attachment => (&self.attachment, "bin"),
            };
            ensure_private_directory(directory)?;
            let key = FileKey::parse(&format!("{}/{token}.{extension}", kind.as_str()))?;
            let final_path = directory.join(key.file_name());
            let temporary_path = directory.join(format!(".{token}.{extension}.part"));
            let file = match create_private_file(&temporary_path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(StorageError::new(StorageOperation::CreateTemporary, &error));
                }
            };
            let mut staged = StagedFile {
                key,
                temporary_path,
                final_path,
                file: Some(file),
                byte_count: 0,
            };
            staged.copy_from(&mut reader, maximum)?;
            return Ok(staged);
        }
        Err(StorageError {
            operation: StorageOperation::CreateTemporary,
            kind: io::ErrorKind::AlreadyExists,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    Body,
    Attachment,
}

impl FileKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::Attachment => "attachment",
        }
    }
}

#[derive(Debug)]
pub(crate) struct StagedFile {
    key: FileKey,
    temporary_path: PathBuf,
    final_path: PathBuf,
    file: Option<File>,
    byte_count: u64,
}

impl StagedFile {
    fn copy_from(&mut self, reader: &mut impl Read, maximum: usize) -> Result<(), StorageError> {
        let file = self.file.as_mut().expect("staged file remains open");
        let mut buffer = [0_u8; IO_BUFFER_BYTES];
        let mut written = 0_usize;
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|error| StorageError::new(StorageOperation::ReadInput, &error))?;
            if read == 0 {
                break;
            }
            let next = written.checked_add(read).ok_or(StorageError {
                operation: StorageOperation::WriteTemporary,
                kind: io::ErrorKind::FileTooLarge,
            })?;
            if next > maximum {
                return Err(StorageError {
                    operation: StorageOperation::WriteTemporary,
                    kind: io::ErrorKind::FileTooLarge,
                });
            }
            file.write_all(&buffer[..read])
                .map_err(|error| StorageError::new(StorageOperation::WriteTemporary, &error))?;
            written = next;
        }
        file.sync_all()
            .map_err(|error| StorageError::new(StorageOperation::SyncTemporary, &error))?;
        self.byte_count = written as u64;
        Ok(())
    }

    fn publish(mut self) -> Result<PublishedFile, StorageError> {
        self.file.take();
        fs::hard_link(&self.temporary_path, &self.final_path)
            .map_err(|error| StorageError::new(StorageOperation::Publish, &error))?;
        let published = PublishedFile {
            key: self.key.clone(),
            path: self.final_path.clone(),
            retained: false,
        };
        if let Err(error) = fs::remove_file(&self.temporary_path) {
            let _ = fs::remove_file(&published.path);
            return Err(StorageError::new(StorageOperation::RemoveTemporary, &error));
        }
        sync_parent_directory(&self.final_path)?;
        Ok(published)
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.temporary_path);
    }
}

#[derive(Debug)]
struct PublishedFile {
    key: FileKey,
    path: PathBuf,
    retained: bool,
}

impl Drop for PublishedFile {
    fn drop(&mut self) {
        if !self.retained {
            let _ = fs::remove_file(&self.path);
            let _ = sync_parent_directory(&self.path);
        }
    }
}

pub(crate) fn prepare_content(
    raw: &[u8],
    staging: &ContentStaging,
    limits: ContentLimits,
) -> Result<PreparedContent, ContentError> {
    preflight_raw(raw, limits)?;
    let message = parser()
        .parse(raw)
        .ok_or(MimeError::Malformed(MalformedMime::NoHeaders))?;
    validate_message_graph(&message, limits)?;

    let subject = bounded_display(
        message.subject().unwrap_or_default(),
        MAX_SUBJECT_BYTES,
        false,
    );
    let sender = message.from().and_then(|address| address.first());
    let sender_name = bounded_display(
        sender
            .and_then(|address| address.name())
            .unwrap_or_default(),
        MAX_ADDRESS_BYTES,
        false,
    );
    let sender_address = bounded_display(
        sender
            .and_then(|address| address.address())
            .unwrap_or_default(),
        MAX_ADDRESS_BYTES,
        false,
    );
    let received_at_ms = message
        .date()
        .filter(|date| date.is_valid())
        .and_then(|date| date.to_timestamp().checked_mul(1_000));

    let (body, body_byte_count, body_truncated) = extract_bounded_body(&message, limits);
    let preview = bounded_prefix(&body, limits.preview_bytes).trim().into();
    let reader_excerpt = bounded_prefix(&body, limits.reader_excerpt_bytes).into();
    // A zero-byte body still gives every durable reservation one physical row.
    let staged_body =
        Some(staging.stage_reader(FileKind::Body, body.as_bytes(), limits.stored_body_bytes)?);

    let mut attachments = Vec::with_capacity(message.attachments.len());
    for (ordinal, part_id) in message.attachments.iter().copied().enumerate() {
        let part = &message.parts[part_id as usize];
        let file_name = bounded_display(
            part.attachment_name().unwrap_or_default(),
            limits.display_file_name_bytes,
            true,
        );
        let media_type = part.content_type().map_or_else(
            || "application/octet-stream".into(),
            |content_type| {
                bounded_display(
                    &format!(
                        "{}/{}",
                        content_type.ctype(),
                        content_type.subtype().unwrap_or("octet-stream")
                    ),
                    MAX_MEDIA_TYPE_BYTES,
                    false,
                )
            },
        );
        let content_id = part
            .content_id()
            .map(|value| bounded_display(value, MAX_CONTENT_ID_BYTES, false))
            .filter(|value| !value.is_empty());
        let disposition = bounded_display(
            part.content_disposition()
                .map(|value| value.ctype())
                .unwrap_or_else(|| {
                    if matches!(part.body, PartType::InlineBinary(_)) {
                        "inline"
                    } else {
                        "attachment"
                    }
                }),
            MAX_DISPOSITION_BYTES,
            false,
        );
        let contents = part.contents();
        let file =
            staging.stage_reader(FileKind::Attachment, contents, limits.decoded_part_bytes)?;
        attachments.push(PreparedAttachment {
            metadata: AttachmentMetadata {
                ordinal: u16::try_from(ordinal).map_err(|_| MimeError::LimitExceeded {
                    resource: MimeResource::Attachments,
                    observed: ordinal.saturating_add(1),
                    maximum: limits.attachments,
                })?,
                file_name,
                media_type,
                content_id,
                disposition,
                byte_count: u64::try_from(contents.len()).unwrap_or(u64::MAX),
            },
            file,
        });
    }

    Ok(PreparedContent {
        metadata: ContentMetadata {
            subject,
            sender_name,
            sender_address,
            received_at_ms,
            preview,
            reader_excerpt,
            body_truncated,
            body_byte_count,
        },
        body: staged_body,
        attachments: attachments.into_boxed_slice(),
    })
}

fn parser() -> &'static MessageParser {
    static PARSER: OnceLock<MessageParser> = OnceLock::new();
    PARSER.get_or_init(|| {
        MessageParser::new()
            .with_mime_headers()
            .header_text(HeaderName::Subject)
            .header_address(HeaderName::From)
            .header_date(HeaderName::Date)
            .default_header_ignore()
    })
}

fn preflight_raw(raw: &[u8], limits: ContentLimits) -> Result<(), MimeError> {
    check_limit(
        MimeResource::RawMessageBytes,
        raw.len(),
        limits.raw_message_bytes,
    )?;
    let header_end = first_header_end(raw).ok_or(MimeError::Malformed(MalformedMime::NoHeaders))?;
    let root_header_fields = count_header_fields(&raw[..header_end])?;
    if root_header_fields == 0 {
        return Err(MimeError::Malformed(MalformedMime::NoHeaders));
    }
    check_limit(
        MimeResource::HeaderBlockBytes,
        header_end,
        limits.header_block_bytes,
    )?;
    check_limit(
        MimeResource::HeaderFields,
        root_header_fields,
        limits.header_fields,
    )?;

    let sections = count_blank_sections(raw, limits.parser_sections)?;
    check_limit(
        MimeResource::ParserSections,
        sections,
        limits.parser_sections,
    )?;

    let allocation_bytes = transfer_allocation_budget(raw, limits.parser_allocation_bytes)?;
    check_limit(
        MimeResource::ParserAllocationBytes,
        allocation_bytes,
        limits.parser_allocation_bytes,
    )?;

    let nested_markers = [
        b"message/rfc822".as_slice(),
        b"message/global",
        b"multipart/digest",
    ]
    .into_iter()
    .try_fold(0_usize, |total, marker| {
        total
            .checked_add(count_ascii_occurrences(raw, marker))
            .ok_or(limit_error(
                MimeResource::NestedMessages,
                usize::MAX,
                limits.nested_messages,
            ))
    })?;
    check_limit(
        MimeResource::NestedMessages,
        nested_markers,
        limits.nested_messages,
    )
}

fn first_header_end(raw: &[u8]) -> Option<usize> {
    let mut index = 0_usize;
    while index < raw.len() {
        if raw.get(index..index + 4) == Some(b"\r\n\r\n") {
            return Some(index + 4);
        }
        if raw.get(index..index + 2) == Some(b"\n\n") {
            return Some(index + 2);
        }
        index += 1;
    }
    None
}

fn count_header_fields(header_block: &[u8]) -> Result<usize, MimeError> {
    let mut fields = 0_usize;
    let mut has_previous = false;
    for raw_line in header_block.split(|&byte| byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if line.is_empty() {
            break;
        }
        if matches!(line.first(), Some(b' ' | b'\t')) {
            if !has_previous {
                return Err(MimeError::Malformed(MalformedMime::InvalidHeaders));
            }
            continue;
        }
        if !line.contains(&b':') {
            return Err(MimeError::Malformed(MalformedMime::InvalidHeaders));
        }
        fields = fields.checked_add(1).ok_or(limit_error(
            MimeResource::HeaderFields,
            usize::MAX,
            usize::MAX - 1,
        ))?;
        has_previous = true;
    }
    Ok(fields)
}

fn count_blank_sections(raw: &[u8], maximum: usize) -> Result<usize, MimeError> {
    let mut sections = 0_usize;
    let mut line_has_content = false;
    for &byte in raw {
        if byte == b'\n' {
            if !line_has_content {
                sections = sections.checked_add(1).ok_or(limit_error(
                    MimeResource::ParserSections,
                    usize::MAX,
                    maximum,
                ))?;
                check_limit(MimeResource::ParserSections, sections, maximum)?;
            }
            line_has_content = false;
        } else if byte != b'\r' {
            line_has_content = true;
        }
    }
    Ok(sections)
}

fn transfer_allocation_budget(raw: &[u8], maximum: usize) -> Result<usize, MimeError> {
    const HEADER: &[u8] = b"content-transfer-encoding:";
    let mut total = 0_usize;
    let mut offset = 0_usize;
    while let Some(position) = find_ascii_case_insensitive(&raw[offset..], HEADER) {
        let absolute = offset + position;
        let remaining = raw.len().saturating_sub(absolute);
        let reservation = remaining.saturating_div(4).saturating_mul(3);
        total = total.checked_add(reservation).ok_or(limit_error(
            MimeResource::ParserAllocationBytes,
            usize::MAX,
            maximum,
        ))?;
        check_limit(MimeResource::ParserAllocationBytes, total, maximum)?;
        offset = absolute.saturating_add(HEADER.len());
    }
    Ok(total)
}

fn count_ascii_occurrences(raw: &[u8], needle: &[u8]) -> usize {
    let mut count = 0_usize;
    let mut offset = 0_usize;
    while let Some(position) = find_ascii_case_insensitive(&raw[offset..], needle) {
        count = count.saturating_add(1);
        offset = offset.saturating_add(position).saturating_add(needle.len());
    }
    count
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let first = needle[0].to_ascii_lowercase();
    let mut offset = 0_usize;
    while offset.saturating_add(needle.len()) <= haystack.len() {
        let relative = haystack[offset..]
            .iter()
            .position(|byte| byte.to_ascii_lowercase() == first)?;
        let candidate = offset + relative;
        if candidate.saturating_add(needle.len()) > haystack.len() {
            return None;
        }
        if haystack[candidate..candidate + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(candidate);
        }
        offset = candidate + 1;
    }
    None
}

#[derive(Default)]
struct ValidationBudget {
    parts: usize,
    edges: usize,
    headers: usize,
    header_bytes: usize,
    decoded_bytes: usize,
    attachments: usize,
    nested_messages: usize,
}

fn validate_message_graph(message: &Message<'_>, limits: ContentLimits) -> Result<(), MimeError> {
    let mut budget = ValidationBudget::default();
    let mut pending = vec![(message, 0_usize)];
    while let Some((message, base_depth)) = pending.pop() {
        if message.parts.is_empty() {
            return Err(MimeError::Malformed(MalformedMime::NoHeaders));
        }
        budget.parts = budget
            .parts
            .checked_add(message.parts.len())
            .ok_or(limit_error(
                MimeResource::MimeParts,
                usize::MAX,
                limits.mime_parts,
            ))?;
        check_limit(MimeResource::MimeParts, budget.parts, limits.mime_parts)?;
        budget.attachments = budget
            .attachments
            .checked_add(message.attachments.len())
            .ok_or(limit_error(
                MimeResource::Attachments,
                usize::MAX,
                limits.attachments,
            ))?;
        check_limit(
            MimeResource::Attachments,
            budget.attachments,
            limits.attachments,
        )?;
        validate_projection_ids(message)?;
        let depths = validate_local_part_graph(message, base_depth, limits, &mut budget)?;

        let raw_len = message.raw_message.len();
        for (index, part) in message.parts.iter().enumerate() {
            let offset_header = part.offset_header as usize;
            let offset_body = part.offset_body as usize;
            let offset_end = part.offset_end as usize;
            if offset_header > offset_body || offset_body > offset_end || offset_end > raw_len {
                return Err(MimeError::Malformed(MalformedMime::InvalidOffsets));
            }
            let block_bytes = offset_body - offset_header;
            check_limit(
                MimeResource::HeaderBlockBytes,
                block_bytes,
                limits.header_block_bytes,
            )?;
            budget.header_bytes =
                budget
                    .header_bytes
                    .checked_add(block_bytes)
                    .ok_or(limit_error(
                        MimeResource::TotalHeaderBytes,
                        usize::MAX,
                        limits.total_header_bytes,
                    ))?;
            check_limit(
                MimeResource::TotalHeaderBytes,
                budget.header_bytes,
                limits.total_header_bytes,
            )?;
            let header_fields =
                count_header_fields(&message.raw_message.as_ref()[offset_header..offset_body])?;
            budget.headers = budget
                .headers
                .checked_add(header_fields)
                .ok_or(limit_error(
                    MimeResource::HeaderFields,
                    usize::MAX,
                    limits.header_fields,
                ))?;
            check_limit(
                MimeResource::HeaderFields,
                budget.headers,
                limits.header_fields,
            )?;
            for header in &part.headers {
                if header.offset_field as usize > header.offset_start as usize
                    || header.offset_start as usize > header.offset_end as usize
                    || header.offset_end as usize > offset_body
                {
                    return Err(MimeError::Malformed(MalformedMime::InvalidOffsets));
                }
            }

            let decoded = match &part.body {
                PartType::Text(value) | PartType::Html(value) => value.len(),
                PartType::Binary(value) | PartType::InlineBinary(value) => value.len(),
                PartType::Message(nested) => nested.raw_message.len(),
                PartType::Multipart(_) => 0,
            };
            check_limit(
                MimeResource::DecodedPartBytes,
                decoded,
                limits.decoded_part_bytes,
            )?;
            budget.decoded_bytes = budget
                .decoded_bytes
                .checked_add(decoded)
                .ok_or(limit_error(
                    MimeResource::DecodedTotalBytes,
                    usize::MAX,
                    limits.decoded_total_bytes,
                ))?;
            check_limit(
                MimeResource::DecodedTotalBytes,
                budget.decoded_bytes,
                limits.decoded_total_bytes,
            )?;

            if let PartType::Message(nested) = &part.body {
                budget.nested_messages =
                    budget.nested_messages.checked_add(1).ok_or(limit_error(
                        MimeResource::NestedMessages,
                        usize::MAX,
                        limits.nested_messages,
                    ))?;
                check_limit(
                    MimeResource::NestedMessages,
                    budget.nested_messages,
                    limits.nested_messages,
                )?;
                let depth = depths[index].checked_add(1).ok_or(limit_error(
                    MimeResource::MimeDepth,
                    usize::MAX,
                    limits.mime_depth,
                ))?;
                check_limit(MimeResource::MimeDepth, depth, limits.mime_depth)?;
                pending.push((nested, depth));
            }
        }
    }
    Ok(())
}

fn validate_projection_ids(message: &Message<'_>) -> Result<(), MimeError> {
    for ids in [
        message.text_body.as_slice(),
        message.html_body.as_slice(),
        message.attachments.as_slice(),
    ] {
        let mut seen = vec![false; message.parts.len()];
        for &id in ids {
            let index = id as usize;
            if index >= message.parts.len() {
                return Err(MimeError::Malformed(MalformedMime::InvalidPartIndex));
            }
            if seen[index] {
                return Err(MimeError::Malformed(MalformedMime::MultipleParents));
            }
            seen[index] = true;
        }
    }
    let mut body = vec![false; message.parts.len()];
    for &id in message.text_body.iter().chain(&message.html_body) {
        if !matches!(
            &message.parts[id as usize].body,
            PartType::Text(_) | PartType::Html(_)
        ) {
            return Err(MimeError::Malformed(MalformedMime::InvalidBodyProjection));
        }
        body[id as usize] = true;
    }
    if message.attachments.iter().any(|&id| body[id as usize]) {
        return Err(MimeError::Malformed(MalformedMime::MultipleParents));
    }
    if message
        .attachments
        .iter()
        .any(|&id| matches!(&message.parts[id as usize].body, PartType::Multipart(_)))
    {
        return Err(MimeError::Malformed(MalformedMime::MultipartAttachment));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Visit {
    Enter { index: usize, depth: usize },
    Exit { index: usize },
}

fn validate_local_part_graph(
    message: &Message<'_>,
    base_depth: usize,
    limits: ContentLimits,
    budget: &mut ValidationBudget,
) -> Result<Vec<usize>, MimeError> {
    let mut colors = vec![0_u8; message.parts.len()];
    let mut parents = vec![0_u8; message.parts.len()];
    let mut depths = vec![base_depth; message.parts.len()];
    let mut stack = vec![Visit::Enter {
        index: 0,
        depth: base_depth,
    }];
    while let Some(visit) = stack.pop() {
        match visit {
            Visit::Enter { index, depth } => {
                if index >= message.parts.len() {
                    return Err(MimeError::Malformed(MalformedMime::InvalidPartIndex));
                }
                match colors[index] {
                    1 => return Err(MimeError::Malformed(MalformedMime::PartCycle)),
                    2 => continue,
                    _ => {}
                }
                check_limit(MimeResource::MimeDepth, depth, limits.mime_depth)?;
                colors[index] = 1;
                depths[index] = depth;
                stack.push(Visit::Exit { index });
                if let PartType::Multipart(children) = &message.parts[index].body {
                    let child_depth = depth.checked_add(1).ok_or(limit_error(
                        MimeResource::MimeDepth,
                        usize::MAX,
                        limits.mime_depth,
                    ))?;
                    check_limit(MimeResource::MimeDepth, child_depth, limits.mime_depth)?;
                    for &child in children.iter().rev() {
                        let child = child as usize;
                        if child >= message.parts.len() {
                            return Err(MimeError::Malformed(MalformedMime::InvalidPartIndex));
                        }
                        parents[child] = parents[child]
                            .checked_add(1)
                            .ok_or(MimeError::Malformed(MalformedMime::MultipleParents))?;
                        if parents[child] > 1 {
                            return Err(MimeError::Malformed(MalformedMime::MultipleParents));
                        }
                        budget.edges = budget.edges.checked_add(1).ok_or(limit_error(
                            MimeResource::MimeParts,
                            usize::MAX,
                            limits.mime_parts,
                        ))?;
                        check_limit(MimeResource::MimeParts, budget.edges, limits.mime_parts)?;
                        stack.push(Visit::Enter {
                            index: child,
                            depth: child_depth,
                        });
                    }
                }
            }
            Visit::Exit { index } => colors[index] = 2,
        }
    }
    if parents[0] != 0 {
        return Err(MimeError::Malformed(MalformedMime::PartCycle));
    }
    if colors.iter().any(|&color| color != 2) {
        return Err(MimeError::Malformed(MalformedMime::UnreachablePart));
    }
    if parents.iter().skip(1).any(|&parents| parents != 1) {
        return Err(MimeError::Malformed(MalformedMime::UnreachablePart));
    }
    Ok(depths)
}

fn extract_bounded_body(message: &Message<'_>, limits: ContentLimits) -> (String, u64, bool) {
    let body_ids = if message.text_body.is_empty() {
        message.html_body.as_slice()
    } else {
        message.text_body.as_slice()
    };
    let mut body = BoundedBody::new(limits);
    let mut source_bytes = 0_u64;
    for &part_id in body_ids {
        let part = &message.parts[part_id as usize];
        let source = match &part.body {
            PartType::Text(text) => text.as_ref(),
            PartType::Html(html) => html.as_ref(),
            _ => {
                body.truncated = true;
                continue;
            }
        };
        source_bytes = source_bytes.saturating_add(u64::try_from(source.len()).unwrap_or(u64::MAX));
        if body.is_full() {
            body.truncated = true;
            continue;
        }
        body.separate_part();
        match &part.body {
            PartType::Text(text) => body.append_plain(text),
            PartType::Html(html) => {
                let html_limit = limits.stored_body_bytes.min(limits.decoded_part_bytes);
                let bounded_html = bounded_prefix(html, html_limit);
                if bounded_html.len() != html.len() {
                    body.truncated = true;
                }
                let text = mail_parser::decoders::html::html_to_text(bounded_html);
                body.append_plain(&text);
            }
            _ => unreachable!("body projection type was checked"),
        }
    }
    (body.output, source_bytes, body.truncated)
}

struct BoundedBody {
    output: String,
    maximum: usize,
    quoted_bytes: usize,
    quoted_maximum: usize,
    truncated: bool,
}

impl BoundedBody {
    fn new(limits: ContentLimits) -> Self {
        Self {
            output: String::with_capacity(limits.stored_body_bytes.min(8 * 1024)),
            maximum: limits.stored_body_bytes,
            quoted_bytes: 0,
            quoted_maximum: limits.quoted_history_bytes,
            truncated: false,
        }
    }

    fn is_full(&self) -> bool {
        self.output.len() >= self.maximum
    }

    fn separate_part(&mut self) {
        if !self.output.is_empty() && !self.output.ends_with('\n') {
            self.push('\n');
        }
    }

    fn append_plain(&mut self, input: &str) {
        for line in input.split_inclusive('\n') {
            if line.trim_start().starts_with('>') {
                let next = self.quoted_bytes.saturating_add(line.len());
                if next > self.quoted_maximum {
                    self.truncated = true;
                    continue;
                }
                self.quoted_bytes = next;
            }
            for character in line.chars() {
                let normalized = match character {
                    '\r' => continue,
                    '\n' | '\t' => character,
                    value if value.is_control() => {
                        self.truncated = true;
                        continue;
                    }
                    value => value,
                };
                if !self.push(normalized) {
                    return;
                }
            }
        }
    }

    fn push(&mut self, character: char) -> bool {
        if self.output.len().saturating_add(character.len_utf8()) > self.maximum {
            self.truncated = true;
            false
        } else {
            self.output.push(character);
            true
        }
    }
}

#[cfg(test)]
fn bound_body(input: &str, limits: ContentLimits) -> (String, bool) {
    let mut body = BoundedBody::new(limits);
    body.append_plain(input);
    (body.output, body.truncated)
}

fn bounded_display(input: &str, maximum: usize, sanitize_path: bool) -> Box<str> {
    let mut output = String::with_capacity(input.len().min(maximum));
    for character in input.chars() {
        let character =
            if character.is_control() || sanitize_path && matches!(character, '/' | '\\' | ':') {
                '_'
            } else {
                character
            };
        if output.len().saturating_add(character.len_utf8()) > maximum {
            break;
        }
        output.push(character);
    }
    output.trim().into()
}

fn bounded_prefix(input: &str, maximum: usize) -> &str {
    if input.len() <= maximum {
        return input;
    }
    let mut end = maximum;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

fn check_limit(resource: MimeResource, observed: usize, maximum: usize) -> Result<(), MimeError> {
    if observed > maximum {
        Err(limit_error(resource, observed, maximum))
    } else {
        Ok(())
    }
}

fn limit_error(resource: MimeResource, observed: usize, maximum: usize) -> MimeError {
    MimeError::LimitExceeded {
        resource,
        observed,
        maximum,
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), StorageError> {
    let created = match fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => {
            return Err(StorageError::new(StorageOperation::CreateDirectory, &error));
        }
    };
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| StorageError::new(StorageOperation::ValidateRoot, &error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(StorageError::invalid(StorageOperation::ValidateRoot));
    }
    set_directory_permissions(path)?;
    if created {
        sync_parent_directory(path)?;
    }
    Ok(())
}

fn validate_published_directory(
    path: &Path,
    operation: StorageOperation,
) -> Result<fs::Metadata, StorageError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| StorageError::new(operation, &error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        Err(StorageError::invalid(operation))
    } else {
        Ok(metadata)
    }
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.file_type() == right.file_type() && left.len() == right.len()
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    if let Err(error) = set_file_permissions(path) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| StorageError::new(StorageOperation::CreateDirectory, &error))
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<(), StorageError> {
    Err(StorageError {
        operation: StorageOperation::ValidateRoot,
        kind: io::ErrorKind::Unsupported,
    })
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), StorageError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| StorageError::new(StorageOperation::SyncDirectory, &error))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), StorageError> {
    Err(StorageError {
        operation: StorageOperation::SyncDirectory,
        kind: io::ErrorKind::Unsupported,
    })
}

fn sync_parent_directory(path: &Path) -> Result<(), StorageError> {
    let parent = path
        .parent()
        .ok_or_else(|| StorageError::invalid(StorageOperation::SyncDirectory))?;
    sync_directory(parent)
}

fn unique_token() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let process = u128::from(std::process::id());
    format!(
        "{:032x}",
        timestamp ^ process.rotate_left(47) ^ u128::from(sequence)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::{MessagePart, PartType};
    use std::{borrow::Cow, io::Cursor};

    const VALID_MULTIPART: &[u8] = b"From: Sender <sender@example.test>\r\n\
Subject: =?UTF-8?Q?Quarterly_=E2=9C=93?=\r\n\
Date: Sat, 20 Nov 2021 14:22:01 -0800\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=mail\r\n\
\r\n\
--mail\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hello, world.\r\n\
> quoted one\r\n\
--mail\r\n\
Content-Type: application/octet-stream; name=../../secret.bin\r\n\
Content-Disposition: attachment; filename=../../secret.bin\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
AQIDBA==\r\n\
--mail--\r\n";

    fn temporary_root(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        std::env::temp_dir().join(format!(
            "nivalis-content-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_root(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    fn staged_file_count(root: &Path) -> usize {
        ["body", "attachment"]
            .into_iter()
            .map(|directory| {
                fs::read_dir(root.join(directory))
                    .unwrap()
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
                    .count()
            })
            .sum()
    }

    fn part(body: PartType<'static>) -> MessagePart<'static> {
        MessagePart {
            headers: Vec::new(),
            is_encoding_problem: false,
            body,
            encoding: mail_parser::Encoding::None,
            offset_header: 0,
            offset_body: 0,
            offset_end: 0,
        }
    }

    fn graph(parts: Vec<MessagePart<'static>>) -> Message<'static> {
        Message {
            html_body: Vec::new(),
            text_body: Vec::new(),
            attachments: Vec::new(),
            parts,
            raw_message: Cow::Borrowed(b""),
        }
    }

    struct FailingReader {
        emitted: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.emitted {
                return Err(io::Error::from(io::ErrorKind::ConnectionReset));
            }
            self.emitted = true;
            let data = b"partial";
            let length = data.len().min(buffer.len());
            buffer[..length].copy_from_slice(&data[..length]);
            Ok(length)
        }
    }

    #[test]
    fn valid_multipart_is_bounded_staged_and_removed_without_commit() {
        let root = temporary_root("valid");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let prepared =
            prepare_content(VALID_MULTIPART, &staging, ContentLimits::default()).unwrap();
        assert_eq!(staged_file_count(&root), 2);

        let published = prepared.publish().unwrap();
        let published_record = published.record();
        assert_eq!(&*published_record.subject, "Quarterly ✓");
        assert_eq!(&*published_record.sender_address, "sender@example.test");
        assert_eq!(&*published_record.preview, "Hello, world.\n> quoted one");
        assert_eq!(published_record.attachments.len(), 1);
        assert_eq!(
            &*published_record.attachments[0].file_name,
            ".._.._secret.bin"
        );
        assert_eq!(published_record.attachments[0].byte_count, 4);
        assert!(
            published_record
                .body_file_key
                .as_ref()
                .is_some_and(|key| key.as_str().starts_with("body/"))
        );
        let body_path = staging
            .resolve(published_record.body_file_key.as_ref().unwrap())
            .unwrap();
        let attachment_path = staging
            .resolve(&published_record.attachments[0].file_key)
            .unwrap();
        assert!(body_path.is_file());
        assert_eq!(fs::read(&attachment_path).unwrap(), [1, 2, 3, 4]);
        drop(published);
        assert!(!body_path.exists());
        assert!(!attachment_path.exists());
        remove_root(&root);
    }

    #[test]
    fn empty_body_keeps_a_durable_zero_byte_reservation_manifest() {
        let root = temporary_root("empty-body");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let prepared = prepare_content(
            b"Subject: headers only\r\n\r\n",
            &staging,
            ContentLimits::default(),
        )
        .unwrap();
        let staged_record = prepared.record();
        assert_eq!(staged_record.body_byte_count, 0);
        assert!(staged_record.body_file_key.is_some());

        let published = prepared.publish().unwrap();
        let published_record = published.record();
        assert_eq!(published_record, staged_record);
        let body_path = staging
            .resolve(published_record.body_file_key.as_ref().unwrap())
            .unwrap();
        assert_eq!(fs::metadata(&body_path).unwrap().len(), 0);
        drop(published);
        assert!(!body_path.exists());
        remove_root(&root);
    }

    #[test]
    fn retained_publication_guard_keeps_body_and_attachments() {
        let root = temporary_root("retain");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let mut published = prepare_content(VALID_MULTIPART, &staging, ContentLimits::default())
            .unwrap()
            .publish()
            .unwrap();
        let record = published.record();
        let body_path = staging
            .resolve(record.body_file_key.as_ref().unwrap())
            .unwrap();
        let attachment_path = staging.resolve(&record.attachments[0].file_key).unwrap();
        published.retain_files();
        drop(published);
        assert!(body_path.is_file());
        assert_eq!(fs::read(&attachment_path).unwrap(), [1, 2, 3, 4]);
        remove_root(&root);
    }

    #[test]
    fn raw_and_header_limits_fail_before_staging() {
        let root = temporary_root("limits");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let limits = ContentLimits {
            raw_message_bytes: 8,
            ..ContentLimits::default()
        };
        let error = prepare_content(b"Subject: x\r\n\r\ny", &staging, limits).unwrap_err();
        assert!(matches!(
            error,
            ContentError::Mime(MimeError::LimitExceeded {
                resource: MimeResource::RawMessageBytes,
                ..
            })
        ));

        let limits = ContentLimits {
            header_block_bytes: 8,
            ..ContentLimits::default()
        };
        let error = prepare_content(b"Subject: x\r\n\r\ny", &staging, limits).unwrap_err();
        assert!(matches!(
            error,
            ContentError::Mime(MimeError::LimitExceeded {
                resource: MimeResource::HeaderBlockBytes,
                ..
            })
        ));
        assert_eq!(staged_file_count(&root), 0);
        remove_root(&root);
    }

    #[test]
    fn preflight_uses_earliest_separator_and_bounds_parser_work() {
        let mixed_separators = b"Subject: x\n\nbody\r\n\r\nlater";
        let earliest = b"Subject: x\n\n".len();
        assert_eq!(first_header_end(mixed_separators), Some(earliest));
        assert_eq!(
            preflight_raw(
                mixed_separators,
                ContentLimits {
                    header_block_bytes: earliest,
                    ..ContentLimits::default()
                }
            ),
            Ok(())
        );

        let sections = b"Subject: x\r\n\r\none\r\n\r\ntwo";
        assert_eq!(
            preflight_raw(
                sections,
                ContentLimits {
                    parser_sections: 2,
                    ..ContentLimits::default()
                }
            ),
            Ok(())
        );
        assert!(matches!(
            preflight_raw(
                sections,
                ContentLimits {
                    parser_sections: 1,
                    ..ContentLimits::default()
                }
            ),
            Err(MimeError::LimitExceeded {
                resource: MimeResource::ParserSections,
                ..
            })
        ));

        let encoded = b"Subject: x\r\nContent-Transfer-Encoding:\r\n base64\r\n\r\nQQ==";
        let exact = transfer_allocation_budget(encoded, usize::MAX).unwrap();
        assert_eq!(
            preflight_raw(
                encoded,
                ContentLimits {
                    parser_allocation_bytes: exact,
                    ..ContentLimits::default()
                }
            ),
            Ok(())
        );
        assert!(matches!(
            preflight_raw(
                encoded,
                ContentLimits {
                    parser_allocation_bytes: exact - 1,
                    ..ContentLimits::default()
                }
            ),
            Err(MimeError::LimitExceeded {
                resource: MimeResource::ParserAllocationBytes,
                ..
            })
        ));
    }

    #[test]
    fn preflight_rejects_deep_unencoded_nested_messages_before_parse() {
        let mut raw = String::from("Subject: nested\r\n\r\n");
        for _ in 0..9 {
            raw.push_str("Content-Type: message/rfc822\r\n\r\n");
        }
        raw.push_str("Subject: leaf\r\n\r\nbody");
        assert!(matches!(
            preflight_raw(raw.as_bytes(), ContentLimits::default()),
            Err(MimeError::LimitExceeded {
                resource: MimeResource::NestedMessages,
                observed: 9,
                maximum: 8,
            })
        ));
    }

    #[test]
    fn corrupted_rfc822_attachment_remains_bounded_and_cleans_staging() {
        let raw = b"From: a@example.com\r\n\
Subject: broken rfc822 attachment\r\n\
Content-Type: multipart/mixed; boundary=BOUND\r\n\
\r\n\
--BOUND\r\n\
Content-Type: text/plain\r\n\
\r\n\
hello\r\n\
--BOUND\r\n\
Content-Type: message/rfc822\r\n\
Content-Disposition: attachment; filename=broken.eml\r\n\
\r\n\
this attachment is not a valid eml, sorry!\r\n\
--BOUND--\r\n";
        let root = temporary_root("corrupt-rfc822");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let prepared = prepare_content(raw, &staging, ContentLimits::default()).unwrap();
        assert!(staged_file_count(&root) <= 2);
        drop(prepared);
        assert_eq!(staged_file_count(&root), 0);
        remove_root(&root);
    }

    #[test]
    fn graph_validation_rejects_cycles_multiple_parents_and_orphans() {
        let limits = ContentLimits::default();
        let self_cycle = graph(vec![part(PartType::Multipart(vec![0]))]);
        assert_eq!(
            validate_message_graph(&self_cycle, limits),
            Err(MimeError::Malformed(MalformedMime::PartCycle))
        );

        let two_cycle = graph(vec![
            part(PartType::Multipart(vec![1])),
            part(PartType::Multipart(vec![0])),
        ]);
        assert_eq!(
            validate_message_graph(&two_cycle, limits),
            Err(MimeError::Malformed(MalformedMime::PartCycle))
        );

        let duplicate = graph(vec![
            part(PartType::Multipart(vec![1, 2])),
            part(PartType::Text(Cow::Borrowed("one"))),
            part(PartType::Multipart(vec![1])),
        ]);
        assert_eq!(
            validate_message_graph(&duplicate, limits),
            Err(MimeError::Malformed(MalformedMime::MultipleParents))
        );

        let orphan = graph(vec![
            part(PartType::Text(Cow::Borrowed("root"))),
            part(PartType::Text(Cow::Borrowed("orphan"))),
        ]);
        assert_eq!(
            validate_message_graph(&orphan, limits),
            Err(MimeError::Malformed(MalformedMime::UnreachablePart))
        );
    }

    #[test]
    fn graph_validation_rejects_out_of_range_and_excess_depth() {
        let out_of_range = graph(vec![part(PartType::Multipart(vec![1]))]);
        assert_eq!(
            validate_message_graph(&out_of_range, ContentLimits::default()),
            Err(MimeError::Malformed(MalformedMime::InvalidPartIndex))
        );

        let deep = graph(vec![
            part(PartType::Multipart(vec![1])),
            part(PartType::Multipart(vec![2])),
            part(PartType::Text(Cow::Borrowed("leaf"))),
        ]);
        let limits = ContentLimits {
            mime_depth: 1,
            ..ContentLimits::default()
        };
        assert!(matches!(
            validate_message_graph(&deep, limits),
            Err(MimeError::LimitExceeded {
                resource: MimeResource::MimeDepth,
                ..
            })
        ));

        let mut invalid_offsets = part(PartType::Text(Cow::Borrowed("body")));
        invalid_offsets.offset_body = 1;
        let invalid_offsets = graph(vec![invalid_offsets]);
        assert_eq!(
            validate_message_graph(&invalid_offsets, ContentLimits::default()),
            Err(MimeError::Malformed(MalformedMime::InvalidOffsets))
        );
    }

    #[test]
    fn graph_validation_rejects_invalid_projection_ids_and_types() {
        for projection in ["text", "html", "attachment"] {
            let mut message = graph(vec![part(PartType::Text(Cow::Borrowed("body")))]);
            match projection {
                "text" => message.text_body.push(1),
                "html" => message.html_body.push(1),
                "attachment" => message.attachments.push(1),
                _ => unreachable!(),
            }
            assert_eq!(
                validate_message_graph(&message, ContentLimits::default()),
                Err(MimeError::Malformed(MalformedMime::InvalidPartIndex)),
                "projection {projection}"
            );
        }

        let mut overlap = graph(vec![part(PartType::Text(Cow::Borrowed("body")))]);
        overlap.text_body.push(0);
        overlap.attachments.push(0);
        assert_eq!(
            validate_message_graph(&overlap, ContentLimits::default()),
            Err(MimeError::Malformed(MalformedMime::MultipleParents))
        );

        let mut invalid_body = graph(vec![part(PartType::Binary(Cow::Borrowed(b"body")))]);
        invalid_body.text_body.push(0);
        assert_eq!(
            validate_message_graph(&invalid_body, ContentLimits::default()),
            Err(MimeError::Malformed(MalformedMime::InvalidBodyProjection))
        );

        let mut multipart_attachment = graph(vec![part(PartType::Multipart(Vec::new()))]);
        multipart_attachment.attachments.push(0);
        assert_eq!(
            validate_message_graph(&multipart_attachment, ContentLimits::default()),
            Err(MimeError::Malformed(MalformedMime::MultipartAttachment))
        );
    }

    #[test]
    fn graph_resource_limits_accept_n_and_reject_n_plus_one() {
        let raw = b"A: 1\r\nB: 2\r\n\r\nx";
        let header_end = first_header_end(raw).unwrap();
        let mut header_part = part(PartType::Text(Cow::Borrowed("x")));
        header_part.offset_body = header_end as u32;
        header_part.offset_end = raw.len() as u32;
        let mut headers = graph(vec![header_part]);
        headers.raw_message = Cow::Borrowed(raw);
        let exact_headers = ContentLimits {
            header_fields: 2,
            total_header_bytes: header_end,
            ..ContentLimits::default()
        };
        assert_eq!(validate_message_graph(&headers, exact_headers), Ok(()));
        assert!(matches!(
            validate_message_graph(
                &headers,
                ContentLimits {
                    header_fields: 1,
                    ..exact_headers
                }
            ),
            Err(MimeError::LimitExceeded {
                resource: MimeResource::HeaderFields,
                ..
            })
        ));
        assert!(matches!(
            validate_message_graph(
                &headers,
                ContentLimits {
                    total_header_bytes: header_end - 1,
                    ..exact_headers
                }
            ),
            Err(MimeError::LimitExceeded {
                resource: MimeResource::TotalHeaderBytes,
                ..
            })
        ));

        let one_part = graph(vec![part(PartType::Text(Cow::Borrowed("abc")))]);
        let exact_decoded = ContentLimits {
            decoded_part_bytes: 3,
            decoded_total_bytes: 3,
            ..ContentLimits::default()
        };
        assert_eq!(validate_message_graph(&one_part, exact_decoded), Ok(()));
        assert!(matches!(
            validate_message_graph(
                &one_part,
                ContentLimits {
                    decoded_part_bytes: 2,
                    ..exact_decoded
                }
            ),
            Err(MimeError::LimitExceeded {
                resource: MimeResource::DecodedPartBytes,
                ..
            })
        ));

        let three_parts = graph(vec![
            part(PartType::Multipart(vec![1, 2])),
            part(PartType::Text(Cow::Borrowed("ab"))),
            part(PartType::Text(Cow::Borrowed("cd"))),
        ]);
        let exact_tree = ContentLimits {
            mime_parts: 3,
            decoded_total_bytes: 4,
            ..ContentLimits::default()
        };
        assert_eq!(validate_message_graph(&three_parts, exact_tree), Ok(()));
        for (limits, resource) in [
            (
                ContentLimits {
                    mime_parts: 2,
                    ..exact_tree
                },
                MimeResource::MimeParts,
            ),
            (
                ContentLimits {
                    decoded_total_bytes: 3,
                    ..exact_tree
                },
                MimeResource::DecodedTotalBytes,
            ),
        ] {
            assert!(matches!(
                validate_message_graph(&three_parts, limits),
                Err(MimeError::LimitExceeded { resource: found, .. }) if found == resource
            ));
        }

        let mut attachment = graph(vec![part(PartType::Binary(Cow::Borrowed(b"x")))]);
        attachment.attachments.push(0);
        assert_eq!(
            validate_message_graph(
                &attachment,
                ContentLimits {
                    attachments: 1,
                    ..ContentLimits::default()
                }
            ),
            Ok(())
        );
        assert!(matches!(
            validate_message_graph(
                &attachment,
                ContentLimits {
                    attachments: 0,
                    ..ContentLimits::default()
                }
            ),
            Err(MimeError::LimitExceeded {
                resource: MimeResource::Attachments,
                ..
            })
        ));

        let nested = graph(vec![part(PartType::Text(Cow::Borrowed("nested")))]);
        let outer = graph(vec![part(PartType::Message(nested))]);
        assert_eq!(
            validate_message_graph(
                &outer,
                ContentLimits {
                    nested_messages: 1,
                    ..ContentLimits::default()
                }
            ),
            Ok(())
        );
        assert!(matches!(
            validate_message_graph(
                &outer,
                ContentLimits {
                    nested_messages: 0,
                    ..ContentLimits::default()
                }
            ),
            Err(MimeError::LimitExceeded {
                resource: MimeResource::NestedMessages,
                ..
            })
        ));
    }

    #[test]
    fn body_bounds_preserve_utf8_and_limit_quoted_history() {
        let limits = ContentLimits {
            stored_body_bytes: 11,
            quoted_history_bytes: 4,
            ..ContentLimits::default()
        };
        let (body, truncated) = bound_body("你好\n>12345\nend", limits);
        assert!(truncated);
        assert_eq!(body, "你好\nend");
        assert!(std::str::from_utf8(body.as_bytes()).is_ok());
        assert_eq!(bounded_prefix("你好", 4), "你");
    }

    #[test]
    fn body_projection_aggregates_parts_and_decodes_legacy_html() {
        let mut message = graph(vec![
            part(PartType::Multipart(vec![1, 2])),
            part(PartType::Text(Cow::Borrowed("one"))),
            part(PartType::Text(Cow::Borrowed("two"))),
        ]);
        message.text_body = vec![1, 2];
        let (body, source_bytes, truncated) =
            extract_bounded_body(&message, ContentLimits::default());
        assert_eq!(body, "one\ntwo");
        assert_eq!(source_bytes, 6);
        assert!(!truncated);

        let raw = b"Subject: legacy html\r\n\
Content-Type: text/html; charset=windows-1252\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
<p>caf=E9</p><p>next</p>";
        let root = temporary_root("legacy-html");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let published = prepare_content(raw, &staging, ContentLimits::default())
            .unwrap()
            .publish()
            .unwrap();
        let record = published.record();
        let path = staging
            .resolve(record.body_file_key.as_ref().unwrap())
            .unwrap();
        let stored = fs::read_to_string(path).unwrap();
        assert!(stored.contains("café"));
        assert!(stored.contains("next"));
        drop(published);
        remove_root(&root);
    }

    #[test]
    fn file_keys_reject_escape_and_stream_limit_removes_temporary_file() {
        for invalid in [
            "",
            "/absolute",
            "../escape",
            "body/../escape",
            "body\\escape",
            "other/file.bin",
            "body/a/b",
            "body//x",
            "body/./x",
            "body/x/",
            "body/00000000000000000000000000000000.bin",
            "body/0000000000000000000000000000000A.txt",
            "body/con.txt",
        ] {
            assert!(FileKey::parse(invalid).is_err(), "accepted {invalid}");
        }
        assert!(FileKey::parse(&"x".repeat(513)).is_err());
        assert!(FileKey::parse("body/00000000000000000000000000000000.txt").is_ok());
        assert!(FileKey::parse("attachment/abcdef0123456789abcdef0123456789.bin").is_ok());

        let root = temporary_root("stream-limit");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let error = staging
            .stage_reader(FileKind::Attachment, Cursor::new([0_u8; 9]), 8)
            .unwrap_err();
        assert_eq!(error.kind, io::ErrorKind::FileTooLarge);
        assert_eq!(staged_file_count(&root), 0);
        remove_root(&root);
    }

    #[test]
    fn streaming_accepts_exact_limit_and_cleans_partial_read_failure() {
        let root = temporary_root("stream-failure");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let exact = staging
            .stage_reader(FileKind::Body, Cursor::new(b"12345678"), 8)
            .unwrap();
        assert_eq!(exact.byte_count, 8);
        drop(exact);
        assert_eq!(staged_file_count(&root), 0);

        let error = staging
            .stage_reader(FileKind::Attachment, FailingReader { emitted: false }, 32)
            .unwrap_err();
        assert_eq!(error.operation, StorageOperation::ReadInput);
        assert_eq!(error.kind, io::ErrorKind::ConnectionReset);
        assert_eq!(staged_file_count(&root), 0);
        remove_root(&root);
    }

    #[test]
    fn publication_conflict_rolls_back_prior_files_without_overwrite() {
        let raw = b"Subject: conflict\r\n\
Content-Type: multipart/mixed; boundary=x\r\n\
\r\n\
--x\r\nContent-Type: text/plain\r\n\r\nbody\r\n\
--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=a.bin\r\n\r\none\r\n\
--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=b.bin\r\n\r\ntwo\r\n\
--x--\r\n";
        let root = temporary_root("publish-conflict");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let prepared = prepare_content(raw, &staging, ContentLimits::default()).unwrap();
        assert_eq!(prepared.attachments.len(), 2);
        let body_path = prepared.body.as_ref().unwrap().final_path.clone();
        let first_attachment_path = prepared.attachments[0].file.final_path.clone();
        let conflict_path = prepared.attachments[1].file.final_path.clone();
        let mut conflict = create_private_file(&conflict_path).unwrap();
        conflict.write_all(b"existing").unwrap();
        conflict.sync_all().unwrap();
        drop(conflict);

        let error = prepared.publish().unwrap_err();
        assert_eq!(error.operation, StorageOperation::Publish);
        assert!(!body_path.exists());
        assert!(!first_attachment_path.exists());
        assert_eq!(fs::read(&conflict_path).unwrap(), b"existing");
        assert_eq!(staged_file_count(&root), 0);
        remove_root(&root);
    }

    #[cfg(unix)]
    #[test]
    fn storage_repairs_permissions_and_rejects_symlink_root() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = temporary_root("permissions");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let staging = ContentStaging::open(root.clone()).unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for directory in ["body", "attachment"] {
            assert_eq!(
                fs::metadata(root.join(directory))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let file = staging
            .stage_reader(FileKind::Body, Cursor::new(b"body"), 4)
            .unwrap();
        assert_eq!(
            fs::metadata(&file.temporary_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let final_path = file.final_path.clone();
        let published = file.publish().unwrap();
        assert_eq!(
            fs::metadata(&final_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(published);
        remove_root(&root);

        let target = temporary_root("symlink-target");
        let link = temporary_root("symlink-link");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();
        assert!(ContentStaging::open(link.clone()).is_err());
        let _ = fs::remove_file(link);
        remove_root(&target);

        for child in ["body", "attachment"] {
            let root = temporary_root("symlink-child");
            let target = temporary_root("symlink-child-target");
            fs::create_dir_all(&root).unwrap();
            fs::create_dir_all(&target).unwrap();
            let target_mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            symlink(&target, root.join(child)).unwrap();
            assert!(ContentStaging::open(root.clone()).is_err());
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                target_mode
            );
            remove_root(&root);
            remove_root(&target);
        }

        let root = temporary_root("file-child");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("body"), b"not a directory").unwrap();
        assert!(ContentStaging::open(root.clone()).is_err());
        remove_root(&root);
    }

    #[test]
    fn published_file_can_be_opened_and_removed_idempotently() {
        let root = temporary_root("published-access");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let staged = staging
            .stage_reader(FileKind::Body, Cursor::new(b"stored body"), 64)
            .unwrap();
        let key = staged.key.clone();
        let mut published = staged.publish().unwrap();
        published.retained = true;

        let mut opened = staging.open_file(&key).unwrap();
        let mut body = String::new();
        opened.read_to_string(&mut body).unwrap();
        assert_eq!(body, "stored body");
        drop(opened);

        assert_eq!(
            staging.remove_published_file(&key).unwrap(),
            RemoveOutcome::Removed
        );
        assert_eq!(
            staging.remove_published_file(&key).unwrap(),
            RemoveOutcome::Missing
        );
        assert!(!staging.resolve(&key).unwrap().exists());

        drop(published);
        remove_root(&root);
    }

    #[cfg(unix)]
    #[test]
    fn published_access_rejects_escape_and_unlinks_symlink_only() {
        use std::os::unix::fs::symlink;

        let root = temporary_root("published-symlink");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let malicious = FileKey("body/../outside.txt".into());
        let open_error = staging.open_file(&malicious).unwrap_err();
        assert_eq!(open_error.operation, StorageOperation::OpenPublished);
        assert_eq!(open_error.kind, io::ErrorKind::InvalidInput);
        let remove_error = staging.remove_published_file(&malicious).unwrap_err();
        assert_eq!(remove_error.operation, StorageOperation::RemovePublished);
        assert_eq!(remove_error.kind, io::ErrorKind::InvalidInput);

        let external = temporary_root("published-external-target");
        fs::write(&external, b"outside target").unwrap();
        let key = FileKey::parse("body/11111111111111111111111111111111.txt").unwrap();
        let link = staging.resolve(&key).unwrap();
        symlink(&external, &link).unwrap();

        let error = staging.open_file(&key).unwrap_err();
        assert_eq!(error.operation, StorageOperation::OpenPublished);
        assert_eq!(error.kind, io::ErrorKind::InvalidInput);
        assert_eq!(fs::read(&external).unwrap(), b"outside target");
        assert_eq!(
            staging.remove_published_file(&key).unwrap(),
            RemoveOutcome::Removed
        );
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read(&external).unwrap(), b"outside target");

        remove_root(&root);
        fs::remove_file(external).unwrap();
    }

    #[test]
    fn published_removal_rejects_and_preserves_directory() {
        let root = temporary_root("published-directory");
        let staging = ContentStaging::open(root.clone()).unwrap();
        let key = FileKey::parse("attachment/22222222222222222222222222222222.bin").unwrap();
        let path = staging.resolve(&key).unwrap();
        fs::create_dir(&path).unwrap();

        let remove_error = staging.remove_published_file(&key).unwrap_err();
        assert_eq!(remove_error.operation, StorageOperation::RemovePublished);
        assert_eq!(remove_error.kind, io::ErrorKind::InvalidInput);
        assert!(path.is_dir());
        let open_error = staging.open_file(&key).unwrap_err();
        assert_eq!(open_error.operation, StorageOperation::OpenPublished);
        assert_eq!(open_error.kind, io::ErrorKind::InvalidInput);
        assert!(path.is_dir());

        remove_root(&root);
    }
}
