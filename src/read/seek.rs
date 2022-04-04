// Copyright (c) 2021 Harry [Majored] [hello@majored.pw]
// MIT License (https://github.com/Majored/rs-async-zip/blob/main/LICENSE)

//! A module for reading ZIP file from a seekable source.
//!
//! # Example
//! ```no_run
//! # use async_zip::read::seek::ZipFileReader;
//! # use tokio::fs::File;
//! # use async_zip::error::ZipError;
//! #
//! # async fn run() -> Result<(), ZipError> {
//! let mut file = File::open("./Archive.zip").await.unwrap();
//! let mut zip = ZipFileReader::new(&mut file).await?;
//!
//! assert_eq!(zip.entries().len(), 2);
//!
//! // Consume the entries out-of-order.
//! let mut reader = zip.entry_reader(1).await?;
//! reader.read_to_string_crc().await?;
//!
//! let mut reader = zip.entry_reader(0).await?;
//! reader.read_to_string_crc().await?;
//! #   Ok(())
//! # }
//! ```

use crate::error::{Result, ZipError};
use crate::read::{CompressionReader, ZipEntry, ZipEntryReader, OwnedReader, PrependReader};
use crate::spec::compression::Compression;
use crate::spec::header::{CentralDirectoryHeader, EndOfCentralDirectoryHeader, LocalFileHeader};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};

use std::io::SeekFrom;
use async_io_utilities::AsyncDelimiterReader;


/// A method which allows ZIP entries to be read both: out-of-order and multiple times.
/// 
/// As a result, this method requries the source to implement both [`AsyncRead`] and [`AsyncSeek`].
pub struct SeekMethod<R: AsyncRead + AsyncSeek + Unpin> {
    pub(crate) reader: R,
    pub(crate) entries: Vec<ZipEntry>,
    pub(crate) comment: Option<String>,
}

impl<R: AsyncRead + AsyncSeek + Unpin> super::ZipFileReader<SeekMethod<R>> {
    /// Constructs a new ZIP archive file reader using the seeking method ([`SeekMethod`]).
    pub async fn new(mut reader: R) -> Result<Self> {
        let (entries, comment) = read_cd(&mut reader).await?;
        let inner =  SeekMethod { reader, entries, comment };

        Ok(super::ZipFileReader { inner })
    }

    /// Returns a shared reference to a list of the ZIP file's entries.
    pub fn entries(&self) -> &Vec<ZipEntry> {
        &self.inner.entries
    }

    /// Searches for an entry with a specific filename.
    pub fn entry(&self, name: &str) -> Option<(usize, &ZipEntry)> {
        for (index, entry) in self.entries().iter().enumerate() {
            if entry.name() == name {
                return Some((index, entry));
            }
        }
        
        None
    }

    /// Returns an optional ending comment.
    pub fn comment(&self) -> Option<&str> {
        self.inner.comment.as_ref().map(|x| &x[..])
    }

    /// Opens an entry at the provided index for reading.
    pub async fn entry_reader(&mut self, index: usize) -> Result<ZipEntryReader<'_, R>> {
        let entry = self.inner.entries.get(index).ok_or(ZipError::EntryIndexOutOfBounds)?;

        self.inner.reader.seek(SeekFrom::Start(entry.offset.unwrap() as u64 + 4)).await?;

        let header = LocalFileHeader::from_reader(&mut self.inner.reader).await?;
        let data_offset = (header.file_name_length + header.extra_field_length) as i64;
        self.inner.reader.seek(SeekFrom::Current(data_offset)).await?;

        if entry.data_descriptor() {
            let delimiter = crate::spec::signature::DATA_DESCRIPTOR.to_le_bytes();
            let reader = OwnedReader::Borrow(&mut self.inner.reader);
            let reader = PrependReader::Normal(reader);
            let reader = AsyncDelimiterReader::new(reader, &delimiter);
            let reader = CompressionReader::from_reader(entry.compression(), reader.take(u64::MAX));

            Ok(ZipEntryReader::with_data_descriptor(entry, reader, false))
        } else {
            let reader = OwnedReader::Borrow(&mut self.inner.reader);
            let reader = PrependReader::Normal(reader);
            let reader = reader.take(entry.compressed_size.unwrap().into());
            let reader = CompressionReader::from_reader(entry.compression(), reader);

            Ok(ZipEntryReader::from_raw(entry, reader, false))
        }
    }
}

pub(crate) async fn read_cd<R: AsyncRead + AsyncSeek + Unpin>(reader: &mut R) -> Result<(Vec<ZipEntry>, Option<String>)> {
    const MAX_ENDING_LENGTH: u64 = (u16::MAX - 2) as u64;

    let length = reader.seek(SeekFrom::End(0)).await?;
    let seek_to = length.saturating_sub(MAX_ENDING_LENGTH);

    reader.seek(SeekFrom::Start(seek_to)).await?;

    let mut comment = None;
    let delimiter = crate::spec::signature::END_OF_CENTRAL_DIRECTORY.to_le_bytes();
    let mut reader = AsyncDelimiterReader::new(reader, &delimiter);

    loop {
        let mut buffer = [0; async_io_utilities::SUGGESTED_BUFFER_SIZE];
        if reader.read(&mut buffer).await? == 0 {
            break;
        }
    }

    if !reader.matched() {
        return Err(ZipError::UnexpectedHeaderError(0, crate::spec::signature::END_OF_CENTRAL_DIRECTORY));
    }

    reader.reset();
    let eocdh = EndOfCentralDirectoryHeader::from_reader(&mut reader).await?;

    // Outdated feature so unlikely to ever make it into this crate.
    if eocdh.disk_num != eocdh.start_cent_dir_disk || eocdh.num_of_entries != eocdh.num_of_entries_disk {
        return Err(ZipError::FeatureNotSupported("Spanned/split files"));
    }
    
    if eocdh.file_comm_length > 0 {
        comment = Some(crate::utils::read_string(&mut reader, eocdh.file_comm_length as usize).await?);
    }

    let reader = reader.into_inner();
    reader.seek(SeekFrom::Start(eocdh.cent_dir_offset.into())).await?;
    let mut entries = Vec::with_capacity(eocdh.num_of_entries.into());

    for _ in 0..eocdh.num_of_entries {
        entries.push(read_cd_entry(reader).await?);
    }

    Ok((entries, comment))
}

pub(crate) async fn read_cd_entry<R: AsyncRead + Unpin>(reader: &mut R) -> Result<ZipEntry> {
    crate::utils::assert_signature(reader, crate::spec::signature::CENTRAL_DIRECTORY_FILE_HEADER).await?;

    let header = CentralDirectoryHeader::from_reader(reader).await?;
    let filename = crate::utils::read_string(reader, header.file_name_length.into()).await?;
    let extra = crate::utils::read_bytes(reader, header.extra_field_length.into()).await?;
    let comment = crate::utils::read_string(reader, header.file_comment_length.into()).await?;

    let entry = ZipEntry {
        name: filename,
        comment: Some(comment),
        data_descriptor: header.flags.data_descriptor,
        crc32: Some(header.crc),
        uncompressed_size: Some(header.uncompressed_size),
        compressed_size: Some(header.compressed_size),
        last_modified: crate::spec::date::zip_date_to_chrono(header.mod_date, header.mod_time),
        extra: Some(extra),
        compression: Compression::from_u16(header.compression)?,
        offset: Some(header.lh_offset),
    };

    Ok(entry)
}
