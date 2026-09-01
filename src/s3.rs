//! Object storage.
//!
//! Two buckets, two jobs. Templates are read out of one, backups are
//! written into the other, and they are usually at different providers
//! because the whole point of a backup is that it is not next to the thing
//! it backs up. RedCraft reads templates from Scaleway and writes backups
//! to Backblaze.
//!
//! The key layout is deliberately the same as the tool this replaces:
//! `<server>.tar` for a template and `<server>.tar.gz` for a backup, flat,
//! no prefix. Changing it would mean the old daemon could no longer be put
//! back, and being able to put it back is the entire safety net during a
//! cutover.

use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("{bucket}/{key} is not there")]
    Missing { bucket: String, key: String },
    #[error("could not read {bucket}/{key}: {message}")]
    Get {
        bucket: String,
        key: String,
        message: String,
    },
    #[error("could not write {bucket}/{key}: {message}")]
    Put {
        bucket: String,
        key: String,
        message: String,
    },
    #[error("no credentials: set {0} in the environment")]
    NoCredentials(&'static str),
}

/// Everything needed to talk to one bucket.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketConfig {
    pub bucket: String,
    /// The provider's S3 endpoint, for example
    /// `https://s3.fr-par.scw.cloud`. Leave it out for real AWS.
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    /// Which environment variables hold the credentials.
    ///
    /// Named rather than inlined, because this config file is meant to be
    /// readable and a config file with a secret key in it is not.
    #[serde(default = "default_access_key_var")]
    pub access_key_var: String,
    #[serde(default = "default_secret_key_var")]
    pub secret_key_var: String,
}

fn default_region() -> String {
    "us-east-1".into()
}
fn default_access_key_var() -> String {
    "AWS_ACCESS_KEY_ID".into()
}
fn default_secret_key_var() -> String {
    "AWS_SECRET_ACCESS_KEY".into()
}

pub struct Bucket {
    client: Client,
    name: String,
}

impl Bucket {
    pub async fn connect(config: &BucketConfig) -> Result<Self, S3Error> {
        let access_key = std::env::var(&config.access_key_var).map_err(|_| {
            S3Error::NoCredentials(Box::leak(config.access_key_var.clone().into_boxed_str()))
        })?;
        let secret_key = std::env::var(&config.secret_key_var).map_err(|_| {
            S3Error::NoCredentials(Box::leak(config.secret_key_var.clone().into_boxed_str()))
        })?;

        let credentials = Credentials::new(access_key, secret_key, None, None, "rcmd");
        let mut builder = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(config.region.clone()))
            .credentials_provider(credentials)
            // Scaleway and Backblaze both want the bucket in the path, not
            // in the hostname.
            .force_path_style(true)
            // Newer SDKs add a checksum header by default that several
            // S3 compatible providers reject outright. Only send one when
            // the operation actually needs it.
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            );
        if let Some(endpoint) = &config.endpoint {
            builder = builder.endpoint_url(endpoint);
        }

        Ok(Self {
            client: Client::from_conf(builder.build()),
            name: config.bucket.clone(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reads an object. A missing key is its own error, because "there is
    /// no template for this server yet" is a normal thing that happens and
    /// not a reason to log a stack trace.
    pub async fn get(&self, key: &str) -> Result<Vec<u8>, S3Error> {
        let response = self
            .client
            .get_object()
            .bucket(&self.name)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                let service = e.into_service_error();
                if service.is_no_such_key() {
                    S3Error::Missing {
                        bucket: self.name.clone(),
                        key: key.to_string(),
                    }
                } else {
                    S3Error::Get {
                        bucket: self.name.clone(),
                        key: key.to_string(),
                        message: service.to_string(),
                    }
                }
            })?;

        let bytes = response
            .body
            .collect()
            .await
            .map_err(|e| S3Error::Get {
                bucket: self.name.clone(),
                key: key.to_string(),
                message: e.to_string(),
            })?
            .into_bytes();
        Ok(bytes.to_vec())
    }

    /// Uploads a file from disk.
    ///
    /// Streamed from the path rather than read into memory first, because a
    /// survival world backup is measured in gigabytes and the box has other
    /// things to do with its RAM.
    pub async fn put_file(&self, key: &str, path: &std::path::Path) -> Result<u64, S3Error> {
        let body = ByteStream::from_path(path)
            .await
            .map_err(|e| S3Error::Put {
                bucket: self.name.clone(),
                key: key.to_string(),
                message: e.to_string(),
            })?;
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

        self.client
            .put_object()
            .bucket(&self.name)
            .key(key)
            .body(body)
            .send()
            .await
            .map_err(|e| S3Error::Put {
                bucket: self.name.clone(),
                key: key.to_string(),
                message: e.into_service_error().to_string(),
            })?;
        Ok(size)
    }
}

/// The key a template lives under. Kept identical to the old daemon's.
pub fn template_key(server: &str) -> String {
    format!("{server}.tar")
}

/// The key a backup goes to.
///
/// By default this is the same key every time, which is what the old daemon
/// did, so a backup overwrites the one before it.
///
/// :warning: That means there is only ever ONE backup per server. Set
/// `key_template` to something with `{date}` in it if you would rather keep
/// history, but be aware that nothing prunes them for you.
pub fn backup_key(template: &str, server: &str, at: chrono::DateTime<chrono::Utc>) -> String {
    template
        .replace("{server}", server)
        .replace("{date}", &at.format("%Y-%m-%d").to_string())
        .replace("{datetime}", &at.format("%Y-%m-%dT%H-%M-%SZ").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn moment() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 9, 1, 6, 30, 0).unwrap()
    }

    #[test]
    fn the_template_key_matches_the_old_daemon_exactly() {
        // The rollback depends on this. If rcmd wrote templates somewhere
        // else, putting rcsm back would find nothing.
        assert_eq!(template_key("hub"), "hub.tar");
        assert_eq!(template_key("crea_build_plot"), "crea_build_plot.tar");
    }

    #[test]
    fn the_default_backup_key_overwrites_like_the_old_one_did() {
        assert_eq!(backup_key("{server}.tar.gz", "hub", moment()), "hub.tar.gz");
    }

    #[test]
    fn a_dated_key_keeps_history_when_you_ask_for_it() {
        assert_eq!(
            backup_key("{server}/{date}.tar.gz", "hub", moment()),
            "hub/2026-09-01.tar.gz"
        );
        assert_eq!(
            backup_key("{server}/{datetime}.tar.gz", "hub", moment()),
            "hub/2026-09-01T06-30-00Z.tar.gz"
        );
    }

    #[test]
    fn a_key_template_with_no_placeholder_is_left_alone() {
        // Not useful, but it must not silently become something else.
        assert_eq!(backup_key("fixed.tar.gz", "hub", moment()), "fixed.tar.gz");
    }
}
