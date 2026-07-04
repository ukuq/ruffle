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
}

impl JavaScriptStorageBackend {
    pub(crate) fn from_window_property(name: &str) -> Option<Self> {
        let window = web_sys::window()?;
        let storage = Reflect::get(window.as_ref(), &JsValue::from_str(name)).ok()?;
        if storage.is_null() || storage.is_undefined() {
            return None;
        }
        Some(Self { storage })
    }

    fn method(&self, name: &str) -> Option<Function> {
        Reflect::get(&self.storage, &JsValue::from_str(name))
            .ok()?
            .dyn_into::<Function>()
            .ok()
    }
}

impl StorageBackend for JavaScriptStorageBackend {
    fn get(&self, name: &str) -> Option<Vec<u8>> {
        let value = self
            .method("get")?
            .call1(&self.storage, &JsValue::from_str(name))
            .ok()?;
        if value.is_null() || value.is_undefined() {
            return None;
        }
        BASE64_STANDARD.decode(value.as_string()?).ok()
    }

    fn put(&mut self, name: &str, value: &[u8]) -> bool {
        self.method("put")
            .and_then(|method| {
                method
                    .call2(
                        &self.storage,
                        &JsValue::from_str(name),
                        &JsValue::from_str(&BASE64_STANDARD.encode(value)),
                    )
                    .ok()
            })
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    fn remove_key(&mut self, name: &str) {
        if let Some(method) = self.method("remove") {
            let _ = method.call1(&self.storage, &JsValue::from_str(name));
        }
    }
}
