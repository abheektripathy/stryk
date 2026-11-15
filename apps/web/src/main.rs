#[cfg(target_arch = "wasm32")]
mod submit;
#[cfg(target_arch = "wasm32")]
mod wallet;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(target_arch = "wasm32")]
fn main() {
    dioxus::launch(web::App);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("Build stryk-web for wasm32-unknown-unknown to run it in a browser.");
}
