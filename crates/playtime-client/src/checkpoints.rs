//! Verified versioned checkpoint manifests. Unsupported bytes remain inspectable.
use crate::{Client, Error, Result};
use playtime_core::RunId;
use playtime_protocol::{
    ReadError,
    api::{Checkpoint, ContentAvailability},
    bridge::{CheckpointIndex, decode_checkpoint_index},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub enum CheckpointIndexRead {
    Unavailable {
        reason: String,
    },
    Supported {
        index: Box<CheckpointIndex>,
        bytes: Vec<u8>,
    },
    UnsupportedSchema {
        received: u32,
        bytes: Vec<u8>,
    },
}
impl Client {
    pub async fn checkpoint_index(
        &self,
        run: &RunId,
        checkpoint: &Checkpoint,
        cancel: &CancellationToken,
    ) -> Result<CheckpointIndexRead> {
        let (id, content) = match &checkpoint.index {
            None => {
                return Ok(CheckpointIndexRead::Unavailable {
                    reason: "not_recorded".into(),
                });
            }
            Some(ContentAvailability::Unavailable { reason }) => {
                return Ok(CheckpointIndexRead::Unavailable {
                    reason: reason.clone(),
                });
            }
            Some(ContentAvailability::Available {
                artifact_id,
                content,
            }) => (artifact_id, content),
        };
        if content.size_bytes > 4 << 20 {
            return Err(Error::LimitExceeded);
        }
        let artifact = self.artifact(run, id).await?;
        if artifact.content != *content
            || artifact.session_id.as_ref() != Some(&checkpoint.checkpoint.session_id)
            || artifact.published_sequence != checkpoint.checkpoint.published_sequence
        {
            return Err(Error::ContentMismatch);
        }
        let mut bytes = Vec::new();
        self.download(run, &artifact, &mut bytes, cancel).await?;
        let text = std::str::from_utf8(&bytes).map_err(|_| Error::InvalidResponse)?;
        match decode_checkpoint_index(text) {
            Ok(index)
                if index.run_id == *run && index.session_id == checkpoint.checkpoint.session_id =>
            {
                Ok(CheckpointIndexRead::Supported {
                    index: Box::new(index),
                    bytes,
                })
            }
            Err(ReadError::UnsupportedVersion { received, .. }) => {
                Ok(CheckpointIndexRead::UnsupportedSchema { received, bytes })
            }
            _ => Err(Error::InvalidResponse),
        }
    }
}
