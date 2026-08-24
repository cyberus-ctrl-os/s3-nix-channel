//! This module tries to abstract the persistent storage backend. The
//! abstraction is not perfect as S3 leaks through pretty heavily. :)

use std::{
    collections::{BTreeMap, BTreeSet},
    {path::Path, time::Duration},
};

use anyhow::{anyhow, bail, Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use axum::{
    body::Bytes,
    http::{self, Method},
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::error::RequestError;

/// The persistent configuration that lives in the S3 bucket as
/// /channels.json.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
struct PersistentChannelsConfig {
    /// The list of all channels we serve. Each channel needs a
    /// corresponding <channel>.json file for configuration in the
    /// bucket.
    channels: Vec<String>,
}

/// The persistent configuration of a single channel.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ChannelConfig {
    /// The latest element in the channel. If this is foo, users can download it as channel/foo.tar.gz.
    pub latest: Option<String>,

    /// The file extension of the files being served. If this is set to ".iso",
    /// the files have to have the form "some-file-name.iso". Multiple periods
    /// in the file_extension are allowed (e.g. ".tar.xz").
    ///
    /// Must include the starting period. Defaults to ".tar.xz" for backward
    /// compatibility.
    #[serde(default = "default_channel_file_extension")]
    pub file_extension: String,

    /// Alternative names for this channel. This can be used for
    /// transparent renames.
    #[serde(default)]
    pub aliases: Vec<String>,

    /// Previous tarballs in this channel.
    #[serde(default)]
    pub previous: Vec<String>,
}

/// Removes duplicate entries from a vector.
///
/// We keep the first unique entry. All duplicate ones are removed. This
/// function retains the order of the remaining elements.
fn remove_duplicates<T>(vec: &mut Vec<T>) -> bool
where
    T: Eq + Ord + Clone,
{
    let original_size = vec.len();
    let mut seen = BTreeSet::new();
    vec.retain(|item| seen.insert(item.clone()));

    original_size != vec.len()
}

impl ChannelConfig {
    pub fn init(file_extension: &str) -> ChannelConfig {
        ChannelConfig {
            file_extension: file_extension.to_owned(),
            ..Default::default()
        }
    }

    /// Remove duplicates in `previous`. This happened for old releases, because
    /// we didn't prevent re-uploading the same file.
    ///
    /// Returns true, if we removed entries.
    pub fn remove_previous_duplicates(&mut self) -> bool {
        remove_duplicates(&mut self.previous)
    }
}

fn default_channel_file_extension() -> String {
    ".tar.xz".to_owned()
}

/// The list of channels we know about and their latest object keys.
#[derive(Debug, Default, Clone)]
pub struct ChannelsConfig {
    /// A mapping from channel name to latest object key.
    channels: BTreeMap<String, ChannelConfig>,
    aliases: BTreeMap<String, String>,
}

impl ChannelsConfig {
    pub fn channels(&self) -> impl Iterator<Item = (&str, &ChannelConfig)> {
        self.channels.iter().map(|(k, v)| (k.as_ref(), v))
    }

    pub fn channel(&self, channel_name: &str, resolve_aliases: bool) -> Option<ChannelConfig> {
        let channel_name = if resolve_aliases {
            self.resolve_alias(channel_name)?
        } else {
            channel_name
        };

        self.channels.get(channel_name).cloned()
    }

    /// Resolves a channel name or its alias to its canonical name.
    pub fn resolve_alias<'a>(&'a self, potential_alias: &'a str) -> Option<&'a str> {
        if self.channels.contains_key(potential_alias) {
            Some(potential_alias)
        } else {
            self.aliases.get(potential_alias).map(|x| x.as_str())
        }
    }
}

pub struct Client {
    client: aws_sdk_s3::Client,
    bucket: String,
}

impl Client {
    /// Open an S3 client with configuration from the environment.
    // TODO Return a custom error type.
    pub async fn new_from_env(bucket: &str) -> Result<Client> {
        let amzn_config = aws_config::load_from_env().await;
        let s3_config = aws_sdk_s3::config::Builder::from(&amzn_config)
            // TODO For minio compat. Should this be configurable?
            .force_path_style(true)
            .build();

        Ok(Self {
            client: aws_sdk_s3::Client::from_conf(s3_config),
            bucket: bucket.to_owned(),
        })
    }

    /// Read a file from S3 into memory. This should only be used for
    /// small files.
    // TODO Return a custom error type.
    async fn read_file(&self, object_key: &str) -> Result<Bytes> {
        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(object_key)
            .send()
            .await
            // TODO Better error.
            .with_context(|| format!("Failed to read: {object_key}"))?;

        Ok(response.body.collect().await?.into_bytes())
    }

    // TODO Return a custom error type.
    pub async fn load_channels_config(&self) -> Result<ChannelsConfig> {
        if !self.file_exists("channels.json").await? {
            warn!("No channels.json in bucket: Starting with empty configuration!");
            return Ok(ChannelsConfig::default());
        }

        let persistent_config: PersistentChannelsConfig =
            serde_json::from_slice(&self.read_file("channels.json").await?)
                .context("Failed to deserialize channels.json")?;

        debug!("Loaded channel config: {persistent_config:?}");

        let mut channels_config = ChannelsConfig::default();

        for channel_name in persistent_config.channels {
            let config_file = format!("{channel_name}.json");
            let channel_config = self
                .read_file(&config_file)
                .await
                .context("Failed to read channel config")
                .and_then(|bytes| {
                    serde_json::from_slice::<ChannelConfig>(&bytes)
                        .context("Failed to deserialize channel configuration")
                });
            match channel_config {
                Ok(channel_config) => {
                    info!(
                        "Channel {channel_name} points to: {}",
                        channel_config.latest.as_deref().unwrap_or("(nothing yet)")
                    );

                    for alias in &channel_config.aliases {
                        channels_config
                            .aliases
                            .insert(alias.clone(), channel_name.clone());
                    }

                    channels_config
                        .channels
                        .insert(channel_name, channel_config);
                }
                Err(err) => {
                    error!("Failed reading configuration for {channel_name:?}, with path {config_file:?} in the bucket. Ignoring.");
                    info!("Context: {}", err);
                    info!("Cause: {}", err.root_cause());
                    continue;
                }
            }
        }

        Ok(channels_config)
    }

    /// Return a signed request for a specific object key in the bucket.
    pub async fn sign_request(
        &self,
        method: http::Method,
        object_key: &str,
    ) -> Result<String, RequestError> {
        use aws_sdk_s3::presigning::PresigningConfig;

        // TODO Should expiration be configurable?
        let presigning_config = PresigningConfig::expires_in(Duration::from_secs(600))
            .map_err(|_e| RequestError::PresignConfigFailure)?;

        let req = match method {
            Method::GET => self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(object_key)
                .presigned(presigning_config)
                .await
                .map_err(|_e| RequestError::PresignFailure {
                    object_key: object_key.to_owned(),
                }),
            Method::HEAD => self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(object_key)
                .presigned(presigning_config)
                .await
                .map_err(|_e| RequestError::PresignFailure {
                    object_key: object_key.to_owned(),
                }),
            unsupported => Err(RequestError::UnsupportedMethod {
                method: unsupported,
            }),
        }?;

        Ok(req.uri().to_owned())
    }

    /// Upload a file to the persistent store. Doesn't update any channel.
    async fn write_file(&self, object_key: &str, file: &Path) -> Result<()> {
        // We would want to stream the file and not load it all in
        // memory, but it results in XAmzContentSHA256Mismatch. :(
        let data = tokio::fs::read(file)
            .await
            .context("Failed to read input file")?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key)
            .body(data.into())
            .send()
            .await
            .with_context(|| format!("Failed to upload file: {}", file.display()))?;

        Ok(())
    }

    async fn write_data(&self, object_key: &str, data: Vec<u8>) -> Result<()> {
        let data = ByteStream::from(data.to_owned());

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(object_key)
            .body(data)
            .send()
            .await
            .context("Failed to upload file")?;

        Ok(())
    }

    async fn file_exists(&self, object_key: &str) -> Result<bool> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(object_key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(err) => {
                if err.as_service_error().map(|e| e.is_not_found()) == Some(true) {
                    Ok(false)
                } else {
                    Err(anyhow!("Failed to check if object exists: {err}"))
                }
            }
        }
    }

    /// Add a channel to the configuration, and seed with stub json config.
    pub async fn add_channel(&self, channel_name: &str, file_extension: &str) -> Result<()> {
        if channel_name == "channels" {
            return Err(anyhow!("Invalid channel name: {channel_name}"));
        }

        if self.file_exists(&format!("{channel_name}.json")).await? {
            return Err(anyhow!("Refusing to overwrite channel: {channel_name}"));
        }

        self.write_data(
            &format!("{channel_name}.json"),
            serde_json::to_vec_pretty(&ChannelConfig::init(file_extension))
                .context("Failed to serialize channel")?,
        )
        .await
        .context("Failed to create channel.")?;

        // Add channel to channels config.
        let mut persistent_config = if self.file_exists("channels.json").await? {
            serde_json::from_slice(&self.read_file("channels.json").await?)
                .context("Failed to deserialize channels.json")?
        } else {
            PersistentChannelsConfig::default()
        };

        persistent_config.channels.push(channel_name.into());

        self.write_data(
            "channels.json",
            serde_json::to_vec_pretty(&persistent_config).context("Failed to serialize channel")?,
        )
        .await.context(format!("Failed to write channels information. {channel_name}.json file was leaked to the bucket."))?;

        Ok(())
    }
    /// Update the channel to point to the given file.
    ///
    /// **Note:** This operation is not concurrency-safe! Clients must
    /// serialize update operations.
    pub async fn update_channel(&self, channel_name: &str, file: &Path) -> Result<()> {
        let channels_config = self.load_channels_config().await?;
        let channel_name = channels_config
            .resolve_alias(channel_name)
            .ok_or_else(|| anyhow!("Failed to find channel: {channel_name}"))?;
        let mut channel = channels_config
            .channel(channel_name, false)
            .ok_or_else(|| anyhow!("Channel {channel_name} does not exit!"))?;

        // We're changing the channel config anyhow, so let's clean it up at the
        // same time.
        if channel.remove_previous_duplicates() {
            info!("Cleaned up duplicate entries in the channel history.")
        }

        // Path::ends_with and Path::extension unfortunately don't do
        // what we need.
        if !file
            .as_os_str()
            .to_str()
            .ok_or_else(|| anyhow!("File name is not valid UTF-8"))?
            .ends_with(&channel.file_extension)
        {
            bail!(
                "Invalid file ending. Only {} is supported: {}",
                channel.file_extension,
                file.display()
            );
        }

        let object_key = file
            .file_name()
            .ok_or_else(|| anyhow!("No file name: {}", file.display()))?
            .to_str()
            .ok_or_else(|| anyhow!("File name needs to be valid UTF-8: {}", file.display()))?
            .to_owned();

        if self.file_exists(&object_key).await? {
            bail!("Refusing to overwrite key: {object_key}");
        }

        let basename = object_key
            .strip_suffix(&channel.file_extension)
            // This unwrap is safe, because we checked the suffix earlier.
            .unwrap()
            .to_owned();

        self.write_file(&object_key, file).await?;

        println!(
            "Updating channel {channel_name} from {} to {}.",
            channel.latest.as_deref().unwrap_or("(nothing)"),
            object_key
        );

        if let Some(previous) = channel.latest.take() {
            channel.previous.push(previous);
        }
        channel.latest = Some(basename);

        self.write_data(
            &format!("{channel_name}.json"),
            serde_json::to_vec_pretty(&channel).context("Failed to serialize channel")?,
        )
        .await.context("Failed to update channel. This leaked the tarball! Remove it manually, if this is an issue.")?;

        Ok(())
    }

    /// Add a new alias to a channel
    ///
    /// **Note:** This operation is not concurrency-safe! Clients must
    /// serialize update operations.
    pub async fn add_alias(&self, channel_name: &str, alias: &str) -> Result<()> {
        let channels_config = self.load_channels_config().await?;

        if let Some(conflicting_channel) = channels_config.resolve_alias(alias) {
            bail!("{alias} is already used for {conflicting_channel}. Refusing to add!");
        }

        let channel_name = channels_config
            .resolve_alias(channel_name)
            .ok_or_else(|| anyhow!("Failed to find channel: {channel_name}"))?;
        let mut channel = channels_config
            .channel(channel_name, false)
            .ok_or_else(|| anyhow!("Channel {channel_name} does not exit!"))?;

        channel.aliases.push(alias.to_owned());

        self.write_data(
            &format!("{channel_name}.json"),
            serde_json::to_vec_pretty(&channel).context("Failed to serialize channel")?,
        )
        .await.context("Failed to update channel. This leaked the tarball! Remove it manually, if this is an issue.")?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_duplicates_works() {
        // Test with empty vec
        let mut vec: Vec<i32> = vec![];
        assert!(!remove_duplicates(&mut vec));
        assert_eq!(vec, Vec::<i32>::new());

        // Test with duplicates
        let mut vec = vec![1, 2, 3, 2, 4, 3, 5];
        assert!(remove_duplicates(&mut vec));
        assert_eq!(vec, vec![1, 2, 3, 4, 5]);

        // Test with no duplicates
        let mut vec = vec![1, 2, 3, 4, 5];
        assert!(!remove_duplicates(&mut vec));
        assert_eq!(vec, vec![1, 2, 3, 4, 5]);

        // Test with single element
        let mut vec = vec![1];
        assert!(!remove_duplicates(&mut vec));
        assert_eq!(vec, vec![1]);

        // Test with all duplicates
        let mut vec = vec![1, 1, 1, 1];
        assert!(remove_duplicates(&mut vec));
        assert_eq!(vec, vec![1]);

        // Test with strings
        let mut vec = vec![
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
        ];
        assert!(remove_duplicates(&mut vec));
        assert_eq!(vec, vec!["a".to_string(), "b".to_string(), "c".to_string()]);

        // Test that order is preserved
        let mut vec = vec![3, 1, 2, 1, 3, 2];
        assert!(remove_duplicates(&mut vec));
        assert_eq!(vec, vec![3, 1, 2]);
    }
}
