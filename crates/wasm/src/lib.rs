//! Browser (wasm) bindings for `mslx-core`.
//!
//! Exposes a single async entry point, [`export_book`], that resolves an MS Learn
//! cert/exam/path reference and assembles the whole EPUB **in the browser**, using the
//! visitor's machine for every fetch and the rasterisation/zip work. The catalog API and
//! all GitHub-raw content send permissive CORS headers, so the engine's fetch loop runs
//! unchanged here - we only swap the native reqwest `Fetcher` for one backed by `fetch`.

use mslx_core::{
    assemble::build_certification_epub, fetch_prebuilt_index, FetchError, Fetcher, ResolveError,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, Response};

/// Zero-field fetcher - trivially `Sync` (the engine bounds require it) and holds no JS
/// handles, so each call builds its own `fetch` request.
struct BrowserFetcher;

#[async_trait::async_trait(?Send)]
impl Fetcher for BrowserFetcher {
    async fn get_json(&self, url: &str) -> Result<String, FetchError> {
        fetch_text(url)
            .await
            .map_err(|message| FetchError { url: url.to_string(), message })
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        fetch_bytes(url)
            .await
            .map_err(|message| FetchError { url: url.to_string(), message })
    }
}

/// One raw `fetch` (defaults to GET). Returns the `Response` regardless of status; only a
/// CORS/network failure rejects.
async fn fetch_raw(url: &str) -> Result<Response, JsValue> {
    let request = Request::new_with_str(url)?;
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window object"))?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request)).await?;
    resp_value
        .dyn_into::<Response>()
        .map_err(|_| JsValue::from_str("fetch did not return a Response"))
}

/// GET `url`, erroring on non-2xx. Tries the URL directly first - the catalog API and
/// GitHub-raw content are CORS-enabled, so that keeps the heavy fetching on the client. If
/// the direct call is *rejected* (CORS/network), retry through our same-origin `/api/fetch`
/// proxy, which covers the few learn.microsoft.com resources (cert pages, badge images) that
/// send no CORS headers. A plain non-2xx (e.g. a legitimately missing file) does NOT trigger
/// the proxy.
async fn do_fetch(url: &str) -> Result<Response, String> {
    let resp = match fetch_raw(url).await {
        Ok(r) => r,
        Err(_) => {
            let encoded = String::from(js_sys::encode_uri_component(url));
            let proxied = format!("/api/fetch?url={encoded}");
            fetch_raw(&proxied).await.map_err(jserr)?
        }
    };
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(resp)
}

async fn fetch_text(url: &str) -> Result<String, String> {
    let resp = do_fetch(url).await?;
    let text = JsFuture::from(resp.text().map_err(jserr)?).await.map_err(jserr)?;
    text.as_string().ok_or_else(|| "response body was not text".to_string())
}

async fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    let resp = do_fetch(url).await?;
    let buf = JsFuture::from(resp.array_buffer().map_err(jserr)?)
        .await
        .map_err(jserr)?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

fn jserr(v: JsValue) -> String {
    v.as_string().unwrap_or_else(|| format!("{v:?}"))
}

/// Export a certification/exam/path to an EPUB, returned as bytes for the caller to download.
///
/// `input` is a Learn URL or bare code (e.g. `az-305`); `locale` like `en-us`; `date_stamp`
/// is `YYYY-MM-DD`, passed in from JS so the engine stays clock-free on wasm. `on_progress` is
/// a JS callback `(msg: string) => void` invoked as each unit/phase is processed, so the page
/// can stream a live status.
#[wasm_bindgen]
pub async fn export_book(
    input: String,
    locale: String,
    date_stamp: String,
    on_progress: js_sys::Function,
) -> Result<js_sys::Uint8Array, JsValue> {
    console_error_panic_hook::set_once();
    let fetcher = BrowserFetcher;
    let progress = |msg: &str| {
        let _ = on_progress.call1(&JsValue::NULL, &JsValue::from_str(msg));
    };
    progress("Building the Learn index\u{2026}");
    // The prebuilt complete index served by the app. No fallback to the live (truncated) tree:
    // a truncated index silently drops public content, so we surface the error instead.
    let index = fetch_prebuilt_index(&fetcher, "/api/content-index")
        .await
        .map_err(map_resolve_err)?;
    progress("Resolving the certification\u{2026}");
    let bytes = build_certification_epub(&fetcher, &index, &input, &locale, &date_stamp, &progress)
        .await
        .map_err(map_resolve_err)?;
    Ok(js_sys::Uint8Array::from(bytes.as_slice()))
}

/// Map a resolve error to a JS error string. "Content unavailable" is tagged so the UI can
/// show a tailored explanation instead of a generic failure.
fn map_resolve_err(e: ResolveError) -> JsValue {
    match e {
        ResolveError::ContentUnavailable(_) => {
            JsValue::from_str(&format!("CONTENT_UNAVAILABLE|{e}"))
        }
        other => JsValue::from_str(&other.to_string()),
    }
}
