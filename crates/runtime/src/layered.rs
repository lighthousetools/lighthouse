//! Files in layers: built-in ones of the instance, and those a tenant or a
//! person created, stored with the settings.

use lighthouse_settings::{FileKind, Layer, Snapshot, TenantSettings};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Something that comes from a file.
pub(crate) trait LayeredFile: Clone {
    /// Reads a stored file. `None` if it is invalid.
    fn read(source: &str, layer: Layer) -> Option<Self>;
    /// Says whether it is in use.
    fn set_state(&mut self, enabled: bool, disabled_by: Option<Layer>);
}

type Cached<T> = (Arc<Snapshot>, Arc<BTreeMap<String, T>>);

/// Resolves files for a user, cached per state of the settings.
pub(crate) struct Layers<T> {
    settings: TenantSettings,
    kind: FileKind,
    cache: Mutex<HashMap<Option<Uuid>, Cached<T>>>,
}

impl<T> std::fmt::Debug for Layers<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Layers")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl<T: LayeredFile> Layers<T> {
    pub(crate) fn new(settings: TenantSettings, kind: FileKind) -> Self {
        Self {
            settings,
            kind,
            cache: Mutex::default(),
        }
    }

    /// The settings the files are stored with.
    pub(crate) fn settings(&self) -> &TenantSettings {
        &self.settings
    }

    /// Built-in files with what a tenant or a person changed, as one user has
    /// them: personal over tenant over built-in.
    pub(crate) fn resolve(
        &self,
        user: Option<Uuid>,
        built_in: &BTreeMap<String, T>,
    ) -> Arc<BTreeMap<String, T>> {
        let snapshot = self.settings.snapshot();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached, files)) = cache.get(&user)
            && Arc::ptr_eq(cached, &snapshot)
        {
            return files.clone();
        }
        let effective = self.settings.effective(user);
        let mut files = built_in.clone();
        for ((kind, name), file) in &effective.files {
            if *kind != self.kind {
                continue;
            }
            match T::read(&file.source, file.layer) {
                Some(mut item) => {
                    item.set_state(file.enabled, file.disabled_by);
                    files.insert(name.clone(), item);
                }
                None => tracing::error!(%name, "skipping an invalid stored file"),
            }
        }
        for ((kind, name), layer) in &effective.files_off {
            if *kind == self.kind
                && let Some(item) = files.get_mut(name)
            {
                item.set_state(false, Some(*layer));
            }
        }
        let files = Arc::new(files);
        cache.retain(|_, (cached, _)| Arc::ptr_eq(cached, &snapshot));
        cache.insert(user, (snapshot, files.clone()));
        files
    }
}
