//! MC 壁纸 · 生图界面的会话数据(左栏历史对话 + 右栏消息),带落盘。
//!
//! 面板不持有任何状态:用户发消息 / 改名 / 置顶 → 面板发事件回来 → 这里改内存 → 整块回填面板。
//! 这个文件负责把这份内存数据存到 `%APPDATA%\MC壁纸\sessions.json` —— 和壁纸库同目录(方案第五节)。
//!
//! 落盘这一步的几个选择,记下来免得日后推翻:
//! - **整份存一个文件**,不是"一个会话一个文件":列表顺序(置顶组在前)和"停在哪一条"都是这份
//!   数据的属性,拆成多文件就得再引一份清单,两份对不上时最难查。会话是 KB 级文本,整份重写
//!   一次也就几毫秒。
//! - **写盘走"临时文件 + 改名"**:直接往 sessions.json 上盖,写到一半掉电就只剩半个 JSON。
//!   对话历史比一次写失败的代价贵得多。
//! - **改一次落一次(每帧至多一次)**:改动的来源只有用户操作,一帧里最多几笔;挂 `dirty` 标记、
//!   让主循环每帧收一次,比在每个动作里各写一遍省事,也不会有哪个动作忘了落。
//! - **读不懂的文件不致命**:挪成 `sessions.json.bad` 备份掉,再开一份空的,日志留一笔。
//!   库目录那套是同一个态度:用户可写的地方,坏文件不能让程序打不开。
//!
//! 以后再长大(标签 / 搜索 / 上千条历史)就换 SQLite —— 和壁纸库同一个迁移点(方案决策 12)。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 落盘格式的版本。加字段不算破坏性改动,改语义时才 +1(和 `mcwp_spec::SCHEMA_VERSION` 一个规矩)。
const SCHEMA_VERSION: u32 = 1;

/// 新会话的名字。第一条消息会把它顶掉(见 `title_from`)。
const NEW_SESSION_TITLE: &str = "新的对话";

/// 一个会话。`id` 是内部身份 —— 列表会重排(置顶会把整组挪到前面),面板那侧的下标随时在变,
/// 认人一律靠 id、不靠下标。
struct Session {
    id: u64,
    title: String,
    messages: Vec<mcwp_ui::ChatMessage>,
    /// 置顶的排在列表最前面
    pinned: bool,
    /// 有未读:行尾挂个圆点,切进这个会话就清掉
    unread: bool,
}

pub struct Sessions {
    /// 显示顺序:置顶的一组在前,两组各自保持创建先后
    items: Vec<Session>,
    /// 当前会话的 **id**(不是下标 —— 置顶会重排)
    active: u64,
    /// 下一个会话的 id
    next_id: u64,
    /// 消息版本号,每改一次 +1:面板靠它把消息列表钉到底部
    revision: i32,
    /// 有改动还没落盘
    dirty: bool,
    /// 会话文件的落点。读不到 APPDATA 时是 `None`,那就只活在内存里(和以前一样)。
    path: Option<PathBuf>,
}

impl Sessions {
    /// 读盘。三种情况各有各的处理:
    /// - 没文件(一次都没用过聊天)= 一份空会话,不报警;
    /// - 文件读不懂 = 备份成 `.bad` 再开空会话;
    /// - 正常 = 按盘上的顺序整块读回来,顺带清理 id 与空标题。
    pub fn load() -> Self {
        let path = sessions_file().ok();
        let Some(file) = path.clone() else {
            tracing::warn!("读不到 APPDATA,会话只活在内存里,退出即散");
            return Self::fresh(None);
        };
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::fresh(path),
            Err(err) => {
                tracing::warn!("读会话文件 {} 失败:{err}", file.display());
                return Self::fresh(path);
            }
        };
        let stored: StoredFile = match serde_json::from_str(&text) {
            Ok(stored) => stored,
            Err(err) => {
                quarantine(&file, &err);
                return Self::fresh(path);
            }
        };
        if stored.schema > SCHEMA_VERSION {
            tracing::warn!(
                "会话文件是新版本(schema={})写的,当前只认到 {SCHEMA_VERSION},按现有字段尽力读",
                stored.schema
            );
        }

        let mut items: Vec<Session> = stored
            .sessions
            .into_iter()
            .map(|session| Session {
                id: session.id,
                title: match session.title.trim() {
                    "" => NEW_SESSION_TITLE.to_string(),
                    title => title.to_string(),
                },
                messages: session
                    .messages
                    .into_iter()
                    .map(StoredMessage::into_ui)
                    .collect(),
                pinned: session.pinned,
                unread: session.unread,
            })
            .collect();
        // 面板永远至少一行(右栏不该为"列表空了"再写一套空状态),空文件也补一个。
        if items.is_empty() {
            items.push(Session::empty(1));
        }
        // 盘上的 id 只有一个用处:找出"上次停在哪一条"。找完就重新发号 —— 手工改过的文件里
        // id 可能重复、可能是 0,而内部认人全靠它,不如每次读盘都重新排一遍。
        let active = items
            .iter()
            .position(|session| session.id == stored.active)
            .unwrap_or(0);
        for (offset, session) in items.iter_mut().enumerate() {
            session.id = offset as u64 + 1;
        }
        let sessions = Self {
            next_id: items.len() as u64 + 1,
            active: items[active].id,
            revision: 0,
            dirty: false,
            path,
            items,
        };
        tracing::info!(
            "读回 {} 个会话(当前是第 {} 个)",
            sessions.items.len(),
            active + 1
        );
        sessions
    }

    /// 一份只有空会话的内存状态。`path` 为 `None` 表示这次运行不落盘。
    fn fresh(path: Option<PathBuf>) -> Self {
        Self {
            items: vec![Session::empty(1)],
            active: 1,
            next_id: 2,
            revision: 0,
            dirty: false,
            path,
        }
    }

    /// 有改动就落盘。主循环每帧叫一次 —— 落盘只发生在用户操作之后,不是每帧都在写。
    ///
    /// 失败**不重试**:这类错误(磁盘满、目录只读)每帧重试只会把日志刷满,而改动会在下一次
    /// 用户操作时再整体落一遍;`dirty` 因此照清。
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let stored = StoredFile {
            schema: SCHEMA_VERSION,
            active: self.active,
            sessions: self
                .items
                .iter()
                .map(|session| StoredSession {
                    id: session.id,
                    title: session.title.clone(),
                    pinned: session.pinned,
                    unread: session.unread,
                    messages: session
                        .messages
                        .iter()
                        .map(StoredMessage::from_ui)
                        .collect(),
                })
                .collect(),
        };
        if let Err(err) = save(path, &stored) {
            tracing::warn!("会话落盘失败(下次改动会再试一遍):{err:?}");
        }
    }

    /// 当前会话在列表里的下标(面板要的是下标,内部存的是 id)。
    ///
    /// 兜底给 0:删除那边保证了列表不空、`active` 一定指得到人,这里只是不给 panic 留机会。
    fn active_index(&self) -> usize {
        self.items
            .iter()
            .position(|session| session.id == self.active)
            .unwrap_or(0)
    }

    /// 交给面板的一整块会话数据。
    pub fn state(&self) -> mcwp_ui::SessionState {
        let active = self.active_index();
        mcwp_ui::SessionState {
            conversations: self
                .items
                .iter()
                .map(|session| mcwp_ui::ConversationItem {
                    heading: session.title.clone().into(),
                    sub: Self::preview(session).into(),
                    pinned: session.pinned,
                    unread: session.unread,
                })
                .collect(),
            current: active,
            title: self.items[active].title.clone(),
            messages: self.items[active].messages.clone(),
            revision: self.revision,
        }
    }

    /// 左栏那一行小字:最后一条消息的开头。
    fn preview(session: &Session) -> String {
        let Some(last) = session.messages.last() else {
            return "还没有消息".to_string();
        };
        let text = last.text.to_string();
        if text.chars().count() > 16 {
            format!("{}…", text.chars().take(16).collect::<String>())
        } else {
            text
        }
    }

    /// 记一条用户消息,返回它落在哪个会话(回复回来时要按 id 找人)。
    ///
    /// **不在这里塞回复**:回复要么来自模型(后台线程,几秒后才到),要么来自"没配模型"的提示,
    /// 两条路都走 `reply()` 落到同一处 —— 省得这里猜是哪一种。
    pub fn send(&mut self, text: &str) -> u64 {
        let active = self.active_index();
        let session = &mut self.items[active];
        let id = session.id;
        if session.messages.is_empty() {
            session.title = title_from(text);
        }
        session.messages.push(mcwp_ui::ChatMessage {
            mine: true,
            text: text.into(),
        });
        // 自己刚说过话的会话不该挂着未读点
        session.unread = false;
        self.changed();
        id
    }

    /// 往指定会话里补一条"对方说的"消息。找不到那个会话(等待期间被删了)就什么也不做,
    /// 返回 `false` —— 调用方拿它决定要不要顺手把壁纸也应用上去。
    pub fn reply(&mut self, id: u64, text: &str) -> bool {
        let active = self.active;
        let Some(session) = self.items.iter_mut().find(|session| session.id == id) else {
            tracing::info!("会话 {id} 已经不在了,这条回复丢掉");
            return false;
        };
        session.messages.push(mcwp_ui::ChatMessage {
            mine: false,
            text: text.into(),
        });
        // 等回复的时候用户可能已经切走了:那就挂个未读点,别让人错过
        if id != active {
            session.unread = true;
        }
        self.changed();
        true
    }

    /// 这个会话还在吗?「继续」之前问一句 —— 用户完全可能在暂停期间把它删了,
    /// 那时候接着跑只会把结果丢进空气里。
    pub fn exists(&self, id: u64) -> bool {
        self.items.iter().any(|session| session.id == id)
    }

    /// 切到左栏的第 `index` 个会话。切过去就当作读过了。
    pub fn select(&mut self, index: usize) {
        let Some(session) = self.items.get_mut(index) else {
            return;
        };
        let id = session.id;
        session.unread = false;
        if id != self.active {
            self.active = id;
        }
        self.changed();
    }

    /// 新建一个空会话并切过去。
    pub fn create(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Session::empty(id));
        self.active = id;
        self.changed();
    }

    /// 改名。名字两端空白清掉;清完是空的当作没改 —— 名字是列表里唯一的抓手,不能空。
    pub fn rename(&mut self, index: usize, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let Some(session) = self.items.get_mut(index) else {
            return;
        };
        if session.title == name {
            return;
        }
        session.title = name.to_string();
        self.changed();
    }

    /// 删掉第 `index` 个会话。删的是当前会话就落到顶上来的那一条(没有就往前退一格);
    /// 一条都不剩时补一个空会话 —— 右栏不该为"列表空了"再写一套空状态。
    pub fn remove(&mut self, index: usize) {
        if index >= self.items.len() {
            return;
        }
        let removed = self.items.remove(index);
        if self.items.is_empty() {
            self.create();
        } else if removed.id == self.active {
            let next = index.min(self.items.len() - 1);
            self.active = self.items[next].id;
        }
        self.changed();
    }

    /// 置顶 / 取消置顶,返回改完之后是不是置顶状态(面板拿它写状态行)。
    ///
    /// 置顶的整组挪到最前面,两组各自保持原来的先后 —— `sort_by_key` 是稳定排序,
    /// 所以只按"是不是置顶"排,组内的先后不会乱。
    pub fn toggle_pin(&mut self, index: usize) -> bool {
        let Some(session) = self.items.get_mut(index) else {
            return false;
        };
        session.pinned = !session.pinned;
        let pinned = session.pinned;
        self.items.sort_by_key(|session| !session.pinned);
        self.changed();
        pinned
    }

    /// 标为未读。故意不切过去、也不动当前会话:它就是个"回头再看"的记号。
    pub fn mark_unread(&mut self, index: usize) {
        let Some(session) = self.items.get_mut(index) else {
            return;
        };
        if !session.unread {
            session.unread = true;
            self.changed();
        }
    }

    /// 收尾:改过了就记一笔版本号 + 标脏,交给主循环的 `flush` 落盘。
    fn changed(&mut self) {
        self.revision += 1;
        self.dirty = true;
    }
}

impl Session {
    fn empty(id: u64) -> Self {
        Self {
            id,
            title: NEW_SESSION_TITLE.to_string(),
            messages: Vec::new(),
            pinned: false,
            unread: false,
        }
    }
}

/// 用第一条消息给会话起名:多空白折成一个空格,掐头 12 个字。
fn title_from(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 12 {
        format!("{}…", flat.chars().take(12).collect::<String>())
    } else {
        flat
    }
}

/// 会话文件:`%APPDATA%\MC壁纸\sessions.json`(和壁纸库同一个目录)。
fn sessions_file() -> Result<PathBuf> {
    Ok(std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("读不到 APPDATA,无法定位会话文件"))?
        .join("MC壁纸")
        .join("sessions.json"))
}

/// 把读不懂的会话文件挪成 `sessions.json.bad`,别让下一次落盘直接把它盖掉。
///
/// Windows 的改名不覆盖已存在的目标,所以先删旧的备份 —— 备份本身失败只记日志:
/// 真正的目的是"别丢数据",做不到就照原样留着(下一次落盘才会盖它)。
fn quarantine(path: &Path, err: &serde_json::Error) {
    let bad = path.with_extension("json.bad");
    let _ = std::fs::remove_file(&bad);
    match std::fs::rename(path, &bad) {
        Ok(()) => tracing::warn!(
            "会话文件解析失败({err}),已备份到 {},本次开一份空的",
            bad.display()
        ),
        Err(rename_err) => tracing::warn!(
            "会话文件解析失败({err}),备份到 {} 也没成:{rename_err}",
            bad.display()
        ),
    }
}

/// 落盘:写临时文件再改名 —— 中途出状况时,sessions.json 要么是旧的、要么是新的,不会是半个。
fn save(path: &Path, stored: &StoredFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建会话目录失败:{}", parent.display()))?;
    }
    // 存成缩进过的 JSON:出问题时能直接打开看,KB 级文本不值当为省这点体积换可读性。
    let text = serde_json::to_string_pretty(stored).context("序列化会话失败")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("写入失败:{}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("改名失败:{}", path.display()))?;
    tracing::debug!("会话已落盘:{}", path.display());
    Ok(())
}

/// 盘上的整份会话。
#[derive(Serialize, Deserialize)]
struct StoredFile {
    /// 老文件没有这个字段时按 1 算
    #[serde(default = "default_schema")]
    schema: u32,
    /// 上次停在哪一条(存的是 id)
    #[serde(default)]
    active: u64,
    #[serde(default)]
    sessions: Vec<StoredSession>,
}

fn default_schema() -> u32 {
    SCHEMA_VERSION
}

#[derive(Serialize, Deserialize)]
struct StoredSession {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    unread: bool,
    #[serde(default)]
    messages: Vec<StoredMessage>,
}

/// 一条消息的落盘形态。
///
/// 故意不复用 `mcwp_ui::ChatMessage`:那个 `mine` 是"给面板画的投影",而盘上该记的是
/// "谁说的这句话" —— 接上模型之后还要区分系统提示、工具结果,投影会变,存盘格式不该跟着乱。
///
/// `role` 写成**字符串**而不是枚举:以后加"system"这类新角色时,老版本读新文件不该整份失败
/// (一个认不出的角色就整份历史搬进 `.bad`,代价太大),认不出的一律当"对方说的"。
#[derive(Serialize, Deserialize)]
struct StoredMessage {
    #[serde(default = "default_role")]
    role: String,
    #[serde(default)]
    text: String,
}

/// 缺 `role` 的消息按回复算:真正要紧的是文本,最坏也就是左右显示反了。
fn default_role() -> String {
    ROLE_ASSISTANT.to_string()
}

const ROLE_USER: &str = "user";
const ROLE_ASSISTANT: &str = "assistant";

impl StoredMessage {
    fn from_ui(message: &mcwp_ui::ChatMessage) -> Self {
        let role = if message.mine {
            ROLE_USER
        } else {
            ROLE_ASSISTANT
        };
        Self {
            role: role.to_string(),
            text: message.text.to_string(),
        }
    }

    fn into_ui(self) -> mcwp_ui::ChatMessage {
        mcwp_ui::ChatMessage {
            mine: self.role == ROLE_USER,
            text: self.text.into(),
        }
    }
}
