use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use js_sys::{Function, Reflect};
use ruffle_core::backend::storage::StorageBackend;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::Storage;

pub struct LocalStorageBackend {
    storage: Storage,
}

impl LocalStorageBackend {
    pub(crate) fn new(storage: Storage) -> Self {
        LocalStorageBackend { storage }
    }
}

impl StorageBackend for LocalStorageBackend {
    fn get(&self, name: &str) -> Option<Vec<u8>> {
        if let Ok(Some(data)) = self.storage.get(name)
            && let Ok(data) = BASE64_STANDARD.decode(data)
        {
            Some(data)
        } else {
            None
        }
    }

    fn put(&mut self, name: &str, value: &[u8]) -> bool {
        self.storage
            .set(name, &BASE64_STANDARD.encode(value))
            .is_ok()
    }

    fn remove_key(&mut self, name: &str) {
        let _ = self.storage.delete(name);
    }
}

pub struct JavaScriptStorageBackend {
    storage: JsValue,
    get: Function,
    put: Function,
    remove: Function,
}

impl JavaScriptStorageBackend {
    pub(crate) fn from_window_property(name: &str) -> Option<Self> {
        let window = web_sys::window()?;
        let storage = Reflect::get(window.as_ref(), &JsValue::from_str(name)).ok()?;
        Self::from_value(storage)
    }

    fn from_value(storage: JsValue) -> Option<Self> {
        if storage.is_null() || storage.is_undefined() {
            return None;
        }

        let get = Self::method(&storage, "get")?;
        let put = Self::method(&storage, "put")?;
        let remove = Self::method(&storage, "remove")?;
        Some(Self {
            storage,
            get,
            put,
            remove,
        })
    }

    fn method(storage: &JsValue, name: &str) -> Option<Function> {
        Reflect::get(storage, &JsValue::from_str(name))
            .ok()?
            .dyn_into::<Function>()
            .ok()
    }
}

impl StorageBackend for JavaScriptStorageBackend {
    fn get(&self, name: &str) -> Option<Vec<u8>> {
        let value = self
            .get
            .call1(&self.storage, &JsValue::from_str(name))
            .ok()?;
        if value.is_null() || value.is_undefined() {
            return None;
        }
        BASE64_STANDARD.decode(value.as_string()?).ok()
    }

    fn put(&mut self, name: &str, value: &[u8]) -> bool {
        self.put
            .call2(
                &self.storage,
                &JsValue::from_str(name),
                &JsValue::from_str(&BASE64_STANDARD.encode(value)),
            )
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    fn remove_key(&mut self, name: &str) {
        let _ = self.remove.call1(&self.storage, &JsValue::from_str(name));
    }
}
