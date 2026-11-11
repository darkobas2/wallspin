//! Remote wallpaper providers + local folder scanning.
use crate::config::{Config, Keys, Provider, Wallhaven};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

pub const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "webp"];
const MAX_DOWNLOAD: u64 = 60 * 1024 * 1024;
const UA: &str = concat!("wallspin/", env!("CARGO_PKG_VERSION"));

/// One downloadable image offered by a provider.
#[derive(Debug, Clone)]
pub struct Item {
    pub url: String,
    /// Tried if `url` fails (e.g. ArtStation 4k -> large).
    pub fallback: Option<String>,
    /// Page URL; used as the ban key.
    pub origin: String,
    pub filename: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .expect("http client")
}

/// Fetch a batch of candidates. `page` is a hint for paginated providers.
/// Returns items and the provider's last page (if paginated).
pub async fn fetch(c: &reqwest::Client, p: &Provider, keys: &Keys, page: u32) -> Result<(Vec<Item>, Option<u32>)> {
    match p {
        Provider::Wallhaven(w) => wallhaven(c, w, &keys.wallhaven, page).await,
        Provider::Artstation { user } => Ok((artstation(c, user).await?, None)),
        Provider::Bing { market } => Ok((bing(c, market).await?, None)),
        Provider::Apod => Ok((apod(c, &keys.nasa).await?, None)),
        Provider::Unsplash { query } => Ok((unsplash(c, query, &keys.unsplash).await?, None)),
        Provider::Favorites | Provider::Folder { .. } => Ok((vec![], None)),
    }
}

// ---------- Wallhaven ----------

#[derive(Deserialize)]
struct WhResp {
    data: Vec<WhItem>,
    meta: Option<WhMeta>,
}
#[derive(Deserialize)]
struct WhItem {
    url: String,
    path: String,
    dimension_x: u32,
    dimension_y: u32,
}
#[derive(Deserialize)]
struct WhMeta {
    last_page: Option<u32>,
}

pub fn wallhaven_params(w: &Wallhaven) -> Vec<(&'static str, String)> {
    let flag = |set: &[String], names: [&str; 3]| -> String {
        names.iter().map(|n| if set.iter().any(|s| s.eq_ignore_ascii_case(n)) { '1' } else { '0' }).collect()
    };
    let mut q = vec![
        ("q", w.query.clone()),
        ("categories", flag(&w.categories, ["general", "anime", "people"])),
        ("purity", flag(&w.purity, ["sfw", "sketchy", "nsfw"])),
        ("sorting", w.sorting.clone()),
        ("order", "desc".into()),
    ];
    if let Some(v) = &w.min_resolution {
        q.push(("atleast", v.clone()));
    }
    if let Some(v) = &w.resolutions {
        q.push(("resolutions", v.clone()));
    }
    if let Some(v) = &w.ratios {
        q.push(("ratios", v.clone()));
    }
    if let Some(v) = &w.top_range {
        q.push(("topRange", v.clone()));
    }
    q
}

async fn wallhaven(c: &reqwest::Client, w: &Wallhaven, key: &str, page: u32) -> Result<(Vec<Item>, Option<u32>)> {
    let mut q = wallhaven_params(w);
    q.push(("page", page.max(1).to_string()));
    if !key.is_empty() {
        q.push(("apikey", key.to_string()));
    }
    let r: WhResp = c
        .get("https://wallhaven.cc/api/v1/search")
        .query(&q)
        .send()
        .await?
        .error_for_status()
        .context("wallhaven search")?
        .json()
        .await?;
    let items = r
        .data
        .into_iter()
        .map(|i| Item {
            filename: i.path.rsplit('/').next().unwrap_or("wallhaven.jpg").to_string(),
            url: i.path,
            fallback: None,
            origin: i.url,
            width: Some(i.dimension_x),
            height: Some(i.dimension_y),
        })
        .collect();
    Ok((items, r.meta.and_then(|m| m.last_page)))
}

// ---------- ArtStation (public RSS) ----------

async fn artstation(c: &reqwest::Client, user: &str) -> Result<Vec<Item>> {
    let user = user.trim().trim_start_matches('@');
    if user.is_empty() || !user.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
        bail!("invalid ArtStation user {user:?}");
    }
    let xml = c
        .get(format!("https://www.artstation.com/{user}.rss"))
        .send()
        .await?
        .error_for_status()
        .context("artstation rss")?
        .text()
        .await?;
    let link_re = regex::Regex::new(r"https://www\.artstation\.com/artwork/[A-Za-z0-9]+").unwrap();
    let img_re = regex::Regex::new(r#"https://cdn[a-z]?\.artstation\.com/p/assets/images/images/[0-9/]+/large/[^"?<\s]+"#)
        .unwrap();
    let mut out = Vec::new();
    for chunk in xml.split("<item>").skip(1) {
        let origin = link_re.find(chunk).map(|m| m.as_str().to_string());
        let mut seen = std::collections::HashSet::new();
        for m in img_re.find_iter(chunk) {
            let large = m.as_str();
            if !seen.insert(large) {
                continue;
            }
            let file = large.rsplit('/').next().unwrap_or("artstation.jpg");
            // Asset id keeps names unique across artworks.
            let id: String = large.split("/images/images/").nth(1).unwrap_or("").split("/large/").next().unwrap_or("")
                .replace('/', "");
            out.push(Item {
                url: large.replace("/large/", "/4k/"),
                fallback: Some(large.to_string()),
                origin: origin.clone().unwrap_or_else(|| large.to_string()),
                filename: format!("artstation-{id}-{file}"),
                width: None,
                height: None,
            });
        }
    }
    Ok(out)
}

// ---------- Bing photo of the day ----------

#[derive(Deserialize)]
struct BingResp {
    images: Vec<BingImg>,
}
#[derive(Deserialize)]
struct BingImg {
    startdate: String,
    urlbase: String,
}

async fn bing(c: &reqwest::Client, market: &str) -> Result<Vec<Item>> {
    let r: BingResp = c
        .get("https://www.bing.com/HPImageArchive.aspx")
        .query(&[("format", "js"), ("idx", "0"), ("n", "8"), ("mkt", market)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(r.images
        .into_iter()
        .map(|i| Item {
            url: format!("https://www.bing.com{}_UHD.jpg", i.urlbase),
            fallback: Some(format!("https://www.bing.com{}_1920x1080.jpg", i.urlbase)),
            origin: format!("bing:{}:{}", market, i.startdate),
            filename: format!("bing-{}-{}.jpg", market, i.startdate),
            width: None,
            height: None,
        })
        .collect())
}

// ---------- NASA APOD ----------

#[derive(Deserialize)]
struct Apod {
    date: String,
    media_type: String,
    url: Option<String>,
    hdurl: Option<String>,
}

async fn apod(c: &reqwest::Client, key: &str) -> Result<Vec<Item>> {
    let key = if key.is_empty() { "DEMO_KEY" } else { key };
    let r: Vec<Apod> = c
        .get("https://api.nasa.gov/planetary/apod")
        .query(&[("api_key", key), ("count", "10"), ("thumbs", "false")])
        .send()
        .await?
        .error_for_status()
        .context("nasa apod")?
        .json()
        .await?;
    Ok(r.into_iter()
        .filter(|a| a.media_type == "image")
        .filter_map(|a| {
            let url = a.hdurl.clone().or(a.url.clone())?;
            let ext = url.rsplit('.').next().filter(|e| IMAGE_EXT.contains(&e.to_lowercase().as_str()))?.to_lowercase();
            Some(Item {
                fallback: a.url.filter(|u| *u != url),
                url,
                origin: format!("https://apod.nasa.gov/apod/ap{}.html", a.date.replace('-', "").get(2..).unwrap_or("")),
                filename: format!("apod-{}.{ext}", a.date),
                width: None,
                height: None,
            })
        })
        .collect())
}

// ---------- Unsplash (needs free access key) ----------

#[derive(Deserialize)]
struct UnsPhoto {
    id: String,
    width: u32,
    height: u32,
    urls: UnsUrls,
    links: UnsLinks,
}
#[derive(Deserialize)]
struct UnsUrls {
    raw: String,
}
#[derive(Deserialize)]
struct UnsLinks {
    html: String,
}

async fn unsplash(c: &reqwest::Client, query: &str, key: &str) -> Result<Vec<Item>> {
    if key.is_empty() {
        bail!("unsplash needs keys.unsplash (free at unsplash.com/developers)");
    }
    let r: Vec<UnsPhoto> = c
        .get("https://api.unsplash.com/photos/random")
        .header("Authorization", format!("Client-ID {key}"))
        .query(&[("query", query), ("orientation", "landscape"), ("count", "10")])
        .send()
        .await?
        .error_for_status()
        .context("unsplash")?
        .json()
        .await?;
    Ok(r.into_iter()
        .map(|p| {
            // Cap at 3840 wide: same quality for wallpapers, far smaller than raw.
            let scale = (3840.0 / p.width as f64).min(1.0);
            Item {
                url: format!("{}&w=3840&fm=jpg&q=90", p.urls.raw),
                fallback: None,
                origin: p.links.html,
                filename: format!("unsplash-{}.jpg", p.id),
                width: Some((p.width as f64 * scale) as u32),
                height: Some((p.height as f64 * scale) as u32),
            }
        })
        .collect())
}

// ---------- download ----------

/// Stream an item to `dir`, never holding the whole image in memory.
pub async fn download(c: &reqwest::Client, item: &Item, dir: &Path) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let name = sanitize_filename(&item.filename);
    let dest = dir.join(&name);
    let part = dir.join(format!(".{name}.part"));
    let mut last_err = anyhow!("no url");
    for url in std::iter::once(&item.url).chain(item.fallback.iter()) {
        match stream_to(c, url, &part).await {
            Ok(()) => {
                tokio::fs::rename(&part, &dest).await?;
                return Ok(dest);
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                last_err = e;
            }
        }
    }
    Err(last_err)
}

async fn stream_to(c: &reqwest::Client, url: &str, path: &Path) -> Result<()> {
    let resp = c.get(url).send().await?.error_for_status()?;
    if resp.content_length().is_some_and(|l| l > MAX_DOWNLOAD) {
        bail!("too large");
    }
    let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    if !ct.is_empty() && !ct.starts_with("image/") && !ct.starts_with("application/octet-stream") {
        bail!("not an image: {ct}");
    }
    let mut f = tokio::fs::File::create(path).await?;
    let mut total = 0u64;
    let mut body = resp.bytes_stream();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        if total > MAX_DOWNLOAD {
            bail!("too large");
        }
        f.write_all(&chunk).await?;
    }
    f.flush().await?;
    // Must parse as an image header.
    imagesize::size(path).map_err(|e| anyhow!("invalid image: {e}"))?;
    Ok(())
}

fn sanitize_filename(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    let s = s.trim_start_matches('.').to_string();
    if s.is_empty() { "image.jpg".into() } else { s.chars().take(150).collect() }
}

// ---------- local ----------

pub fn is_image(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| IMAGE_EXT.contains(&e.to_lowercase().as_str()))
}

/// Images under `dir` (up to 3 levels deep), skipping hidden/partial files.
pub fn scan_dir(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u8, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')) {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() && depth > 0 => walk(&p, depth - 1, out),
                Ok(t) if t.is_file() && is_image(&p) => out.push(p),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 3, &mut out);
    out
}

pub fn dims_ok(cfg: &Config, w: u32, h: u32) -> bool {
    w >= cfg.min_width && h >= cfg.min_height && (!cfg.landscape_only || w >= h)
}

/// Delete oldest downloads until the folder is under quota. Never touches `keep`.
pub fn enforce_quota(cfg: &Config, keep: &[PathBuf]) -> Result<usize> {
    let limit = cfg.quota_mb.saturating_mul(1024 * 1024);
    if limit == 0 {
        return Ok(0);
    }
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = scan_dir(&cfg.download_folder)
        .into_iter()
        .filter_map(|p| {
            let m = std::fs::metadata(&p).ok()?;
            Some((m.modified().ok()?, m.len(), p))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= limit {
        return Ok(0);
    }
    let target = limit / 100 * 95;
    files.sort_by_key(|f| f.0);
    let mut removed = 0;
    for (_, len, p) in files {
        if total <= target {
            break;
        }
        if keep.contains(&p) {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
            removed += 1;
        }
    }
    Ok(removed)
}
