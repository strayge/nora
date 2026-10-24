// Copyright (c) 2026 strayge
// SPDX-License-Identifier: MIT

//! Backend views preserve logical protocol keys while isolating physical storage.

use super::*;

struct ScopedBackend {
    inner: Arc<dyn StorageBackend>,
    prefix: String,
}

impl Storage {
    /// A scope cannot escape to siblings, even through internal listings or streaming reads.
    pub(crate) fn scoped(&self, prefix: &str) -> Result<Self> {
        validate_storage_key(prefix)?;
        Ok(Self {
            inner: Arc::new(ScopedBackend {
                inner: Arc::clone(&self.inner),
                prefix: format!("{prefix}/"),
            }),
        })
    }
}

impl ScopedBackend {
    fn key(&self, key: &str) -> Result<String> {
        validate_storage_key(key)?;
        let physical = format!("{}{key}", self.prefix);
        validate_storage_key(&physical)?;
        Ok(physical)
    }

    fn listing(&self, prefix: &str) -> Result<String> {
        if !prefix.is_empty() {
            validate_storage_key(listing_key(prefix))?;
        }
        let physical = format!("{}{prefix}", self.prefix);
        validate_storage_key(listing_key(&physical))?;
        Ok(physical)
    }

    fn logical(&self, key: String) -> Option<String> {
        key.strip_prefix(&self.prefix).map(str::to_owned)
    }
}

#[async_trait]
impl StorageBackend for ScopedBackend {
    async fn put(&self, key: &str, data: &[u8], sha256: &str) -> Result<()> {
        self.inner.put(&self.key(key)?, data, sha256).await
    }

    async fn get(&self, key: &str) -> Result<(Bytes, Option<String>)> {
        self.inner.get(&self.key(key)?).await
    }

    async fn pin(&self, key: &str) -> Option<String> {
        self.inner.pin(&self.key(key).ok()?).await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(&self.key(key)?).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .inner
            .list(&self.listing(prefix)?)
            .await?
            .into_iter()
            .filter_map(|key| self.logical(key))
            .collect())
    }

    async fn list_with_meta(&self, prefix: &str) -> Result<Vec<(String, FileMeta)>> {
        Ok(self
            .inner
            .list_with_meta(&self.listing(prefix)?)
            .await?
            .into_iter()
            .filter_map(|(key, meta)| self.logical(key).map(|key| (key, meta)))
            .collect())
    }

    async fn stat(&self, key: &str) -> Option<FileMeta> {
        self.inner.stat(&self.key(key).ok()?).await
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        self.inner.exists(&self.key(key)?).await
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }

    async fn total_size(&self) -> u64 {
        self.inner
            .list_with_meta(&self.prefix)
            .await
            .map(|entries| {
                entries
                    .into_iter()
                    .filter(|(key, _)| key.starts_with(&self.prefix))
                    .map(|(_, meta)| meta.size)
                    .sum()
            })
            .unwrap_or(0)
    }

    fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }

    async fn put_from_path(&self, key: &str, src: &Path, sha256: Option<&str>) -> Result<()> {
        self.inner.put_from_path(&self.key(key)?, src, sha256).await
    }

    async fn copy(&self, src: &str, dst: &str, sha256: Option<&str>) -> Result<()> {
        self.inner
            .copy(&self.key(src)?, &self.key(dst)?, sha256)
            .await
    }

    async fn get_reader(
        &self,
        key: &str,
    ) -> Result<(u64, Option<String>, Pin<Box<dyn AsyncRead + Send + Unpin>>)> {
        self.inner.get_reader(&self.key(key)?).await
    }

    async fn get_range(
        &self,
        key: &str,
        start: u64,
        end: u64,
    ) -> Result<(u64, Option<String>, Pin<Box<dyn AsyncRead + Send + Unpin>>)> {
        self.inner.get_range(&self.key(key)?, start, end).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn logical_keys_round_trip_without_crossing_scope_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        for root in [
            Storage::new_local(dir.path().to_str().unwrap()),
            Storage::from_backend(Arc::new(ObjectStorage::in_memory())),
        ] {
            let first = root.scoped("repositories/a/content").unwrap();
            let second = root.scoped("repositories/a-other/content").unwrap();
            let key = "pypi/pkg/pkg-1.tar.gz";
            first.put(key, b"first").await.unwrap();
            second.put(key, b"second").await.unwrap();
            root.put("pypi/pkg/pkg-1.tar.gz", b"legacy").await.unwrap();
            assert_eq!(first.list("").await.unwrap(), [key]);
            for (listed, meta) in first.list_with_meta("").await.unwrap() {
                assert_eq!(meta.size, 5);
                assert_eq!(first.get(&listed).await.unwrap().as_ref(), b"first");
            }
            first
                .put(".nora-origins/pypi/pkg", b"origin")
                .await
                .unwrap();
            assert_eq!(first.list("").await.unwrap(), [key]);
            assert_eq!(
                first.list_internal(".nora-origins/").await.unwrap(),
                [".nora-origins/pypi/pkg"]
            );
            assert!(!root
                .list("")
                .await
                .unwrap()
                .iter()
                .any(|key| key.contains(".nora-origins")));
            assert_eq!(root.list_internal("repositories/").await.unwrap().len(), 1);
            let (size, pin, mut reader) = first.get_reader(key).await.unwrap();
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            assert_eq!((size, bytes.as_slice()), (5, &b"first"[..]));
            assert_eq!(pin, first.pin(key).await);
            let (_, _, mut range) = first.get_range(key, 1, 3).await.unwrap();
            bytes.clear();
            range.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes, b"irs");
            first.copy(key, "pypi/pkg/copy.tar.gz", None).await.unwrap();
            assert_eq!(
                first.pin(key).await,
                first.pin("pypi/pkg/copy.tar.gz").await
            );
            first.delete(key).await.unwrap();
            assert_eq!(second.get(key).await.unwrap().as_ref(), b"second");
            assert_eq!(root.get(key).await.unwrap().as_ref(), b"legacy");
            assert!(first
                .get("../a-other/content/pypi/pkg/pkg-1.tar.gz")
                .await
                .is_err());
            assert!(root.scoped("").is_err());
            assert!(root.scoped("repositories/a/").is_err());
            let temp = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(temp.path(), b"streamed").unwrap();
            let sha = hex::encode(Sha256::digest(b"streamed"));
            first
                .put_from_path("pypi/pkg/stream.tar.gz", temp.path(), Some(&sha))
                .await
                .unwrap();
            assert_eq!(
                first.get("pypi/pkg/stream.tar.gz").await.unwrap().as_ref(),
                b"streamed"
            );
            assert_eq!(first.total_size().await, 5 + 6 + 8);
        }
    }
}
