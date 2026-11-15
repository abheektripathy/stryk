use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use solana_address::Address;
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{CustomEvent, CustomEventInit, Event};

#[derive(Clone, Debug, PartialEq)]
pub struct Connected {
    pub address: Address,
    wallet: JsValue,
    account: JsValue,
}

pub struct Discovery {
    wallets: Array,
    _register: Closure<dyn FnMut(Event)>,
    _api: Closure<dyn FnMut(JsValue) -> JsValue>,
}

impl Discovery {
    pub fn start() -> Result<Self, String> {
        let window = web_sys::window().ok_or("Browser unavailable")?;
        let wallets = Array::new();
        let list = wallets.clone();
        let api = Closure::<dyn FnMut(JsValue) -> JsValue>::new(move |wallet| {
            if get(&wallet, "features").is_ok() && !list.includes(&wallet, 0) {
                list.push(&wallet);
            }
            let remove = list.clone();
            Closure::once_into_js(move || {
                let index = remove.index_of(&wallet, 0);
                if index >= 0 {
                    remove.splice_many(index as u32, 1, &[]);
                }
            })
        });
        let adapter = Object::new();
        Reflect::set(&adapter, &JsValue::from_str("register"), api.as_ref()).map_err(js_error)?;
        let register_adapter = adapter.clone();
        let register = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            if let Some(event) = event.dyn_ref::<CustomEvent>() {
                if let Ok(callback) = event.detail().dyn_into::<Function>() {
                    let _ = callback.call1(&JsValue::UNDEFINED, &register_adapter);
                }
            }
        });
        window
            .add_event_listener_with_callback(
                "wallet-standard:register-wallet",
                register.as_ref().unchecked_ref(),
            )
            .map_err(js_error)?;
        let init = CustomEventInit::new();
        init.set_detail(&adapter);
        let event = CustomEvent::new_with_event_init_dict("wallet-standard:app-ready", &init)
            .map_err(js_error)?;
        window.dispatch_event(&event).map_err(js_error)?;
        Ok(Self {
            wallets,
            _register: register,
            _api: api,
        })
    }

    pub fn wallets(&self) -> Vec<(String, JsValue)> {
        self.wallets
            .iter()
            .filter_map(|wallet| {
                let features = get(&wallet, "features").ok()?;
                get(&features, "standard:connect").ok()?;
                get(&features, "solana:signTransaction").ok()?;
                get(&wallet, "name")
                    .ok()?
                    .as_string()
                    .map(|name| (name, wallet))
            })
            .collect()
    }

    #[cfg(feature = "local-wallet")]
    pub fn install_fixture(&self, chain: &str) -> Result<(), String> {
        use ed25519_dalek::{Signer, SigningKey};

        let window = web_sys::window().ok_or("Browser unavailable")?;
        if chain != "solana:localnet"
            || window.location().hostname().ok().as_deref() != Some("127.0.0.1")
            || self
                .wallets()
                .iter()
                .any(|(name, _)| name == "Local demo wallet")
        {
            return Ok(());
        }
        let secret = SigningKey::from_bytes(&[42; 32]);
        let address = Address::new_from_array(secret.verifying_key().to_bytes());
        let account = Object::new();
        let chains = Array::of1(&JsValue::from_str(chain));
        let account_features = Array::of1(&JsValue::from_str("solana:signTransaction"));
        for (key, value) in [
            ("address", JsValue::from_str(&address.to_string())),
            ("publicKey", Uint8Array::from(address.as_ref()).into()),
            ("chains", chains.into()),
            ("features", account_features.into()),
        ] {
            Reflect::set(&account, &JsValue::from_str(key), &value).map_err(js_error)?;
        }
        let accounts = Array::of1(&account);
        let connected = Object::new();
        Reflect::set(&connected, &JsValue::from_str("accounts"), &accounts).map_err(js_error)?;
        let connect = Closure::<dyn FnMut() -> JsValue>::new(move || connected.clone().into());
        let sign = Closure::<dyn FnMut(JsValue) -> JsValue>::new(move |input| {
            let signed = (|| -> Result<JsValue, String> {
                if get(&input, "chain")?.as_string().as_deref() != Some("solana:localnet") {
                    return Err("Local fixture cannot sign for another cluster".into());
                }
                if !web_sys::window()
                    .ok_or("Browser unavailable")?
                    .confirm_with_message(
                        "Sign this Stryk transaction with disposable localnet funds?",
                    )
                    .map_err(js_error)?
                {
                    return Err("Wallet signature rejected".into());
                }
                let mut bytes = Uint8Array::new(&get(&input, "transaction")?).to_vec();
                if bytes.len() <= 65 || bytes.len() > 1232 || bytes[0] != 1 {
                    return Err("Invalid single-signer fixture transaction".into());
                }
                let signature = secret.sign(&bytes[65..]).to_bytes();
                bytes[1..65].copy_from_slice(&signature);
                let output = Object::new();
                Reflect::set(
                    &output,
                    &JsValue::from_str("signedTransaction"),
                    &Uint8Array::from(bytes.as_slice()),
                )
                .map_err(js_error)?;
                Ok(Array::of1(&output).into())
            })();
            match signed {
                Ok(signed) => signed,
                Err(error) => Promise::reject(&JsValue::from_str(&error)).into(),
            }
        });
        let connect_feature = Object::new();
        Reflect::set(
            &connect_feature,
            &JsValue::from_str("connect"),
            connect.as_ref(),
        )
        .map_err(js_error)?;
        let sign_feature = Object::new();
        Reflect::set(
            &sign_feature,
            &JsValue::from_str("signTransaction"),
            sign.as_ref(),
        )
        .map_err(js_error)?;
        Reflect::set(
            &sign_feature,
            &JsValue::from_str("supportedTransactionVersions"),
            &Array::of1(&JsValue::from_str("legacy")),
        )
        .map_err(js_error)?;
        let features = Object::new();
        Reflect::set(
            &features,
            &JsValue::from_str("standard:connect"),
            &connect_feature,
        )
        .map_err(js_error)?;
        Reflect::set(
            &features,
            &JsValue::from_str("solana:signTransaction"),
            &sign_feature,
        )
        .map_err(js_error)?;
        let wallet = Object::new();
        Reflect::set(
            &wallet,
            &JsValue::from_str("name"),
            &JsValue::from_str("Local demo wallet"),
        )
        .map_err(js_error)?;
        Reflect::set(&wallet, &JsValue::from_str("features"), &features).map_err(js_error)?;
        Reflect::set(&wallet, &JsValue::from_str("accounts"), &accounts).map_err(js_error)?;
        self.wallets.push(&wallet);
        connect.forget();
        sign.forget();
        Ok(())
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            let _ = window.remove_event_listener_with_callback(
                "wallet-standard:register-wallet",
                self._register.as_ref().unchecked_ref(),
            );
        }
    }
}

impl Connected {
    pub async fn connect(wallet: JsValue, chain: &str) -> Result<Self, String> {
        let features = get(&wallet, "features")?;
        let feature = get(&features, "standard:connect")?;
        let response = call(&feature, "connect", None).await?;
        let accounts = Array::from(&get(&response, "accounts")?);
        for account in accounts.iter() {
            let chains = Array::from(&get(&account, "chains")?);
            let features = Array::from(&get(&account, "features")?);
            if !chains.includes(&JsValue::from_str(chain), 0)
                || !features.includes(&JsValue::from_str("solana:signTransaction"), 0)
            {
                continue;
            }
            let address = get(&account, "address")?
                .as_string()
                .ok_or("Wallet account is missing its address")?
                .parse::<Address>()
                .map_err(|_| "Invalid wallet address")?;
            if Uint8Array::new(&get(&account, "publicKey")?).to_vec() == address.as_ref() {
                return Ok(Self {
                    address,
                    wallet,
                    account,
                });
            }
        }
        Err(format!("Wallet has no account supporting {chain}"))
    }

    pub fn is_current(&self, chain: &str) -> bool {
        let Ok(accounts) = get(&self.wallet, "accounts") else {
            return false;
        };
        Array::from(&accounts).iter().any(|account| {
            let Ok(address) = get(&account, "address") else {
                return false;
            };
            let Ok(chains) = get(&account, "chains") else {
                return false;
            };
            address.as_string().as_deref() == Some(&self.address.to_string())
                && Array::from(&chains).includes(&JsValue::from_str(chain), 0)
        })
    }

    pub async fn sign(&self, chain: &str, transaction: &[u8]) -> Result<Vec<u8>, String> {
        if !self.is_current(chain) {
            return Err("Wallet account changed. Reconnect to continue.".into());
        }
        let features = get(&self.wallet, "features")?;
        let feature = get(&features, "solana:signTransaction")?;
        let versions = Array::from(&get(&feature, "supportedTransactionVersions")?);
        if !versions.includes(&JsValue::from_str("legacy"), 0) {
            return Err("Wallet does not support legacy Solana transactions".into());
        }
        let input = Object::new();
        for (key, value) in [
            ("transaction", Uint8Array::from(transaction).into()),
            ("account", self.account.clone()),
            ("chain", JsValue::from_str(chain)),
        ] {
            Reflect::set(&input, &JsValue::from_str(key), &value).map_err(js_error)?;
        }
        let result = call(&feature, "signTransaction", Some(&input)).await?;
        let results = Array::from(&result);
        if results.length() != 1 || !self.is_current(chain) {
            return Err("Wallet account or signing response changed".into());
        }
        Ok(Uint8Array::new(&get(&results.get(0), "signedTransaction")?).to_vec())
    }
}

fn get(object: &JsValue, key: &str) -> Result<JsValue, String> {
    let value = Reflect::get(object, &JsValue::from_str(key)).map_err(js_error)?;
    if value.is_null() || value.is_undefined() {
        return Err(format!("Wallet feature {key} is unavailable"));
    }
    Ok(value)
}

async fn call(object: &JsValue, method: &str, input: Option<&JsValue>) -> Result<JsValue, String> {
    let function = get(object, method)?
        .dyn_into::<Function>()
        .map_err(|_| "Invalid wallet feature")?;
    let promise = match input {
        Some(input) => function.call1(object, input),
        None => function.call0(object),
    }
    .map_err(js_error)?;
    JsFuture::from(Promise::resolve(&promise))
        .await
        .map_err(js_error)
}

fn js_error(error: JsValue) -> String {
    error
        .as_string()
        .or_else(|| get(&error, "message").ok()?.as_string())
        .unwrap_or_else(|| "Wallet request failed".into())
}
