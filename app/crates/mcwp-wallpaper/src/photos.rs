//! MC 壁纸 · 照片壁纸的本地库 —— `%APPDATA%\MC壁纸\photos` 的读写。
//!
//! 一张壁纸 = **三个文件**,平铺在一个目录里(不建子目录 —— 一张才两三个文件,
//! 一层目录用资源管理器就能手工救):
//! ```text
//! <id>.json        元数据(标题 / 提示词 / 动效配方 / 调色板 / 尺寸 / 生成时间)
//! <id>.png|jpg|bin 画面本身,生图接口原样返回的字节
//! <id>.thumb.png   缩略图,入库时顺手缩好的
//! ```
//!
//! 为什么不是"把元数据塞进 PNG 的 tEXt 块"、为什么不是"一个 JSON 存全部":
//! 见 `mcwp-spec/src/photo.rs` 顶部的取舍。这里只需要记住两条落地规则:
//!
//! - **元数据是"这张壁纸存在"的唯一标志**。扫库只认 `.json`;没有配套图片的条目直接跳过。
//! - 写入顺序**先图后元数据**。反过来的话,中途挂掉会在库里留一个指向空气的条目,
//!   而"有图没元数据"最多是少一张壁纸(图还在盘上,手写一个 JSON 就能救回来)。
//!
//! ## 两个踩过的坑
//!
//! - **库目录换了名字**(早期叫 `library`,里面存的是 `WallpaperSpec` JSON)。旧目录里的
//!   `.json` 一律**没有配套图片**,所以就算同处一个目录也会被扫库安静跳过 —— 但那样日志里
//!   每次扫库都会刷一串"找不到图片文件"的警告。换目录比"兼容旧格式"干净:那批文件本来
//!   就不可用(它们的画面要靠本地着色器现算,而那套已经拆了)。
//! - **缩略图必须入库时就生成**。面板刷一次库要几十张缩略图,现场解 1080p 原图再缩
//!   就是每张 100 ms 起步(一屏要好几秒),而缩略图是几百字节的 PNG,读它就无所谓了。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mcwp_image::Bitmap;
use mcwp_spec::Photo;

/// 缩略图的尺寸上限(16:9)。面板那一格是 168 × 94,按 2 倍给,够清楚也不占地方;
/// 换面板格子尺寸时这个数可以不动 —— 它只是"缩到不超过",不是"必须正好"。
const THUMB_WIDTH: u32 = 320;
const THUMB_HEIGHT: u32 = 180;

/// 库目录。和会话、模型配置同处 `%APPDATA%\MC壁纸`。
pub fn root() -> Result<PathBuf> {
    Ok(std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("读不到 APPDATA,无法定位壁纸库目录"))?
        .join("MC壁纸")
        .join("photos"))
}

/// 库里的一条:元数据 + 三个文件各自的位置。
pub struct Entry {
    pub photo: Photo,
    /// 画面文件(`<id>.png` / `<id>.jpg` / `<id>.bin`)
    pub image: PathBuf,
    /// 元数据文件(`<id>.json`)
    pub meta: PathBuf,
    /// 缩略图(`<id>.thumb.png`)。**可能不存在** —— 手工拷进来的条目就没有,
    /// 那时 `read_thumb` 会拿原图现缩一张(见那里)。
    pub thumb: PathBuf,
}

impl Entry {
    /// 解码画面。这是**唯一**会把 1080p 图读进内存的地方。
    pub fn read_image(&self) -> Result<Bitmap> {
        Bitmap::decode_file(&self.image)
    }

    /// 读缩略图,给面板用。
    ///
    /// 缩略图不在(手工拷进来的条目、或者早期版本存的)就**当场拿原图缩一张并存下去** ——
    /// 花这一次,以后每次刷库都快。存不下去也不致命(比如目录只读):照样返回缩好的像素,
    /// 只是下次还得再缩一遍。
    pub fn read_thumb(&self) -> Result<Bitmap> {
        if self.thumb.is_file() {
            return Bitmap::decode_file(&self.thumb);
        }
        let small = self.read_image()?.thumbnail(THUMB_WIDTH, THUMB_HEIGHT);
        match small.encode_png() {
            Ok(bytes) => {
                if let Err(err) = std::fs::write(&self.thumb, &bytes) {
                    tracing::warn!("缩略图存不下({}):{err}", self.thumb.display());
                }
            }
            Err(err) => tracing::warn!("缩略图编不出来({}):{err}", self.thumb.display()),
        }
        Ok(small)
    }
}

/// 按文件头认图片格式。只认我们要的两种 —— 生图服务商返回的不是 PNG 就是 JPEG,
/// 别的格式落到 `bin`,一样能存能解,只是名字诚实一点。
fn extension_of(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else {
        "bin"
    }
}

/// 存一张新壁纸。`bytes` 是生图接口原样返回的图片字节 —— **直接落盘,不重新编码**:
/// 重压一次只会让画面更糊,而且白花一次 CPU。
///
/// `bitmap` 是调用方已经解好的像素(它本来就要拿去做纹理),这里只从它上面缩一张缩略图 ——
/// 免得为了缩略图再解一遍原图。
pub fn save(photo: &Photo, bytes: &[u8], bitmap: &Bitmap) -> Result<Entry> {
    let dir = root()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建壁纸库目录失败:{}", dir.display()))?;

    let mut stored = photo.clone();
    stored.normalize();
    let image = dir.join(format!("{}.{}", stored.id, extension_of(bytes)));
    let meta = dir.join(format!("{}.json", stored.id));
    let thumb = dir.join(format!("{}.thumb.png", stored.id));

    std::fs::write(&image, bytes)
        .with_context(|| format!("写入图片失败:{}", image.display()))?;
    // 缩略图失败**不致命**:面板那边会回落到"读原图现缩",只是慢一点
    match bitmap.thumbnail(THUMB_WIDTH, THUMB_HEIGHT).encode_png() {
        Ok(bytes) => {
            if let Err(err) = std::fs::write(&thumb, &bytes) {
                tracing::warn!("写缩略图失败({}):{err}", thumb.display());
            }
        }
        Err(err) => tracing::warn!("缩略图编不出来:{err}"),
    }
    // 元数据走"临时文件 + 改名"(和会话 / 模型配置一个态度):半截 JSON 比没有 JSON 难查得多
    let temp = meta.with_extension("json.tmp");
    std::fs::write(&temp, stored.to_pretty_json())
        .with_context(|| format!("写入元数据失败:{}", temp.display()))?;
    std::fs::rename(&temp, &meta)
        .with_context(|| format!("替换元数据失败:{}", meta.display()))?;

    Ok(Entry {
        photo: stored,
        image,
        meta,
        thumb,
    })
}

/// 删掉一条(画面 + 元数据 + 缩略图)。
///
/// **故意只按这条记录自己记着的三个路径删**,不去按 id 通配:`id` 里出现过 `.` 或别的
/// 通配字符的话,通配符就删到别人头上去了。少一个文件不报错(本来就可能没有缩略图),
/// 但三个都删不掉要报出来 —— 那说明目录只读或者文件被占着,用户得知道。
pub fn remove(entry: &Entry) -> Result<()> {
    let mut failed = Vec::new();
    for path in [&entry.image, &entry.meta, &entry.thumb] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => failed.push(format!("{}:{err}", path.display())),
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("有文件删不掉 —— {}", failed.join(";"))
    }
}

/// 扫一遍库目录,**最近生成的排最前**。坏条目只记一条日志跳过,不能让一块坏文件把整个库打不开。
pub fn scan() -> Vec<Entry> {
    let root = match root() {
        Ok(root) => root,
        Err(err) => {
            tracing::warn!("定位壁纸库失败:{err}");
            return Vec::new();
        }
    };
    // 一次都没存过时目录还不存在,这是正常状态,不是错误
    let Ok(dir) = std::fs::read_dir(&root) else {
        return Vec::new();
    };

    let mut entries: Vec<Entry> = dir
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|meta| match read_entry(&meta) {
            Ok(entry) => Some(entry),
            Err(err) => {
                tracing::warn!("跳过壁纸库里的 {}:{err}", meta.display());
                None
            }
        })
        .collect();
    // 按生成时间倒序;时间一样(同一秒连生两张)时用 id 兜底,保证顺序稳定 ——
    // 否则每次扫库卡片顺序可能变,点第 3 张的时候点到的可能是另一张。
    entries.sort_by(|left, right| {
        right
            .photo
            .created
            .cmp(&left.photo.created)
            .then_with(|| right.photo.id.cmp(&left.photo.id))
    });
    entries
}

/// 读一条元数据,并确认配套的图片真的在。读不出 / 图片不在都返回 `Err`,由调用方决定是跳过还是报给用户。
pub fn read_entry(meta: &Path) -> Result<Entry> {
    let text = std::fs::read_to_string(meta).context("读元数据失败")?;
    let photo = Photo::parse(&text).context("解析元数据失败")?;
    let dir = meta.parent().unwrap_or(Path::new("."));
    let image = image_for(dir, &photo.id)
        .ok_or_else(|| anyhow::anyhow!("找不到 {} 对应的图片文件", photo.id))?;
    Ok(Entry {
        thumb: dir.join(format!("{}.thumb.png", photo.id)),
        photo,
        image,
        meta: meta.to_path_buf(),
    })
}

/// 在一个目录里按 id 找图片文件。扩展名按"先 PNG 后 JPEG"的顺序试 —— 两个都只是名字,
/// 真正的格式由 `mcwp-image` 按文件头认。
fn image_for(dir: &Path, id: &str) -> Option<PathBuf> {
    ["png", "jpg", "bin"]
        .into_iter()
        .map(|ext| dir.join(format!("{id}.{ext}")))
        .find(|candidate| candidate.is_file())
}

/// 找一张壁纸。参数可以是库里的 id,也可以是一个 `.json` 文件的路径(手工导进来的也能用)。
pub fn load(id_or_path: &str) -> Result<Entry> {
    let raw = id_or_path.trim();
    if raw.is_empty() {
        anyhow::bail!("没给壁纸 id");
    }
    let direct = Path::new(raw);
    if direct.is_file() {
        return read_entry(direct);
    }
    let meta = root()?.join(format!("{raw}.json"));
    if !meta.is_file() {
        anyhow::bail!("壁纸库里没有 {raw}(先看看 `%APPDATA%\\MC壁纸\\photos` 里有什么)");
    }
    read_entry(&meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 按文件头认扩展名() {
        assert_eq!(extension_of(&[0x89, b'P', b'N', b'G', 0x0D]), "png");
        assert_eq!(extension_of(&[0xFF, 0xD8, 0xFF, 0xE0]), "jpg");
        assert_eq!(extension_of(b"RIFF....WEBP"), "bin");
        assert_eq!(extension_of(&[]), "bin");
    }

    /// 缩略图那条路走的是 `mcwp-image`,这里只钉住"上限值本身合理":
    /// 面板那一格是 168 × 94,缩略图比它小就没法看了。
    #[test]
    fn 缩略图比面板那一格大() {
        assert!(THUMB_WIDTH as f32 >= 168.0 * 1.5);
        assert!(THUMB_HEIGHT as f32 >= 94.0 * 1.5);
    }

    /// 库目录必须和会话、模型配置在同一个 `MC壁纸` 下面,而且**不能**再叫 `library`
    /// (那是旧格式(一份 `WallpaperSpec` JSON)的地盘,混在一起每次扫库都会刷一串跳过的警告)。
    #[test]
    fn 库目录是_photos() {
        let dir = root().expect("测试机上应该读得到 APPDATA");
        assert_eq!(dir.file_name().and_then(|name| name.to_str()), Some("photos"));
        assert_eq!(
            dir.parent().and_then(|parent| parent.file_name()).and_then(|name| name.to_str()),
            Some("MC壁纸")
        );
    }
}
