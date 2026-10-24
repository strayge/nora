// Copyright (c) 2026 The NORA Authors
// SPDX-License-Identifier: MIT

//! A producer's verdict gates the last byte, independently of persistent caching.

use super::{ProxyError, TempFileGuard};
use axum::body::Bytes;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub(crate) struct SpoolStatus {
    pub written: u64,
    pub done: Option<Result<(), String>>,
}

pub(crate) struct SpoolFile {
    pub path: std::path::PathBuf,
    pub file: tokio::fs::File,
    pub guard: TempFileGuard,
}

impl SpoolFile {
    pub(crate) async fn create(temp_dir: &std::path::Path) -> Result<Self, ProxyError> {
        let path = temp_dir.join(format!("proxy-{}", uuid::Uuid::new_v4()));
        let guard = TempFileGuard::new(path.clone());
        let file = tokio::fs::File::create(&path)
            .await
            .map_err(|error| ProxyError::Network(format!("temp file create: {error}")))?;
        Ok(Self { path, file, guard })
    }

    pub(crate) async fn follower(&self) -> std::io::Result<tokio::fs::File> {
        tokio::fs::File::open(&self.path).await
    }
}

/// Flushed bytes can flow early, but no consumer completes before the producer's verdict.
pub(crate) fn spool_follower(
    file: tokio::fs::File,
    status: tokio::sync::watch::Receiver<SpoolStatus>,
    start: u64,
    stall: Duration,
) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send {
    const READ_BUF: usize = 256 * 1024;
    const HOLDBACK_BYTES: u64 = 1;
    const MAX_EMPTY_READS: u32 = 1_000;
    futures::stream::unfold(
        (file, status, start, 0u32),
        move |(mut file, mut status, mut offset, mut empty_reads)| async move {
            use tokio::io::AsyncReadExt;
            loop {
                let current = status.borrow_and_update().clone();
                let released = match &current.done {
                    Some(Ok(())) => current.written,
                    Some(Err(reason)) => {
                        return Some((
                            Err(std::io::Error::other(format!(
                                "upstream blob fetch failed: {reason}"
                            ))),
                            (file, status, offset, empty_reads),
                        ))
                    }
                    None => current.written.saturating_sub(HOLDBACK_BYTES),
                };
                if offset < released {
                    let want = usize::try_from(released - offset)
                        .unwrap_or(READ_BUF)
                        .min(READ_BUF);
                    let mut buf = vec![0u8; want];
                    match file.read(&mut buf).await {
                        Ok(0) => {
                            empty_reads += 1;
                            if empty_reads > MAX_EMPTY_READS {
                                return Some((
                                    Err(std::io::Error::other(format!(
                                        "spool published {} bytes but only {} were readable",
                                        current.written, offset
                                    ))),
                                    (file, status, offset, empty_reads),
                                ));
                            }
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        Ok(n) => {
                            empty_reads = 0;
                            offset += n as u64;
                            buf.truncate(n);
                            return Some((
                                Ok(Bytes::from(buf)),
                                (file, status, offset, empty_reads),
                            ));
                        }
                        Err(error) => {
                            return Some((Err(error), (file, status, offset, empty_reads)))
                        }
                    }
                    continue;
                }
                if current.done.is_some() {
                    return None;
                }
                match tokio::time::timeout(stall, status.changed()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        return Some((
                            Err(std::io::Error::other("upstream blob fetch abandoned")),
                            (file, status, offset, empty_reads),
                        ))
                    }
                    Err(_) => {
                        return Some((
                            Err(std::io::Error::other(format!(
                                "upstream blob fill reported nothing for {stall:?}"
                            ))),
                            (file, status, offset, empty_reads),
                        ))
                    }
                }
            }
        },
    )
}
