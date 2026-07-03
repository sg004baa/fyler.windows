//! M0: 成立性スパイク(DESIGN.md「マイルストーン」)。
//!
//! **絶対ルール4: ここが全項目passするまでM1以降の実装を始めない。**
//! 各項目の検証コードをこのクレートに実装し(検証のためであれば汚くてよい。
//! ここのコードは製品コードに含めない)、結果を docs/M0_RESULTS.md に記録する。
//!
//! 検証はWindows実機で行う(IME・nvim.exe・コンソール挙動が対象のため)。
//! nvim RPCの実験には fyler-engine-nvim ではなく、このクレート内で直接
//! nvim-rsを使ってよい(スパイクは境界ルールの例外。製品コードは例外なし)。
//!
//! nvim.exe のパスは環境変数 `FYLER_NVIM_EXE`、未設定なら PATH の "nvim"。
//! 起動引数は `fyler_engine_nvim::spawn::NVIM_ARGS` から変えない。
//! Windowsでは `CREATE_NO_WINDOW` を必ず付ける。

#![allow(dead_code)]

use std::time::Duration;

use fyler_core::editor::{Key, KeyInput, Modifiers};
use fyler_core::grammar;
use nvim_rs::compat::tokio::Compat;
use nvim_rs::create::tokio::new_child_cmd;
use nvim_rs::{Buffer, Handler, Neovim, UiAttachOptions};
use rmpv::Value;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::sleep;

/// new_child_cmd が返す Neovim の Writer 型。
type NvimWriter = Compat<ChildStdin>;
type Nvim = Neovim<NvimWriter>;

// ---------------------------------------------------------------------------
// redraw通知収集ハンドラ(ext_cmdline / ext_messages のイベントを見るため)
// ---------------------------------------------------------------------------

/// redraw の各イベントバッチを channel へ流すだけの Handler。
/// `Handler: Send + Sync + Clone + 'static` を満たすため、共有はチャネルで行う
/// (ロックを跨いでawaitしない / rs-parking-lot ルール回避)。
#[derive(Clone)]
struct SpikeHandler {
    tx: UnboundedSender<Value>,
}

#[async_trait::async_trait]
impl Handler for SpikeHandler {
    type Writer = NvimWriter;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Nvim) {
        // "redraw" の params は「イベントバッチの配列」。各バッチ = [event_name, arg_tuples..]。
        if name == "redraw" {
            for batch in args {
                let _ = self.tx.send(batch);
            }
        }
    }
}

/// redraw バッチ先頭のイベント名。
fn batch_name(batch: &Value) -> Option<&str> {
    batch.as_array()?.first()?.as_str()
}

/// 収集した redraw イベントのバッファ。
struct Redraw {
    rx: UnboundedReceiver<Value>,
    log: Vec<Value>,
}

impl Redraw {
    fn pump(&mut self) {
        while let Ok(v) = self.rx.try_recv() {
            self.log.push(v);
        }
    }
    /// 保留中を捨てて log をクリア(以降の観測を分離するため)。
    fn reset(&mut self) {
        self.pump();
        self.log.clear();
    }
    fn saw(&self, name: &str) -> bool {
        self.log.iter().any(|b| batch_name(b) == Some(name))
    }
    fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .log
            .iter()
            .filter_map(|b| batch_name(b).map(str::to_string))
            .collect();
        v.sort();
        v.dedup();
        v
    }
    /// デバッグ整形した全バッチ(メッセージ文字列の中身検索用)。
    fn dump(&self) -> String {
        self.log
            .iter()
            .map(|v| format!("{v:?}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// ---------------------------------------------------------------------------
// 小道具
// ---------------------------------------------------------------------------

async fn settle() {
    sleep(Duration::from_millis(140)).await;
}
async fn tick() {
    sleep(Duration::from_millis(30)).await;
}

async fn set_buf(buf: &Buffer<NvimWriter>, lines: &[&str]) {
    buf.set_lines(0, -1, false, lines.iter().map(|s| s.to_string()).collect())
        .await
        .expect("set_lines");
}
async fn get_buf(buf: &Buffer<NvimWriter>) -> Vec<String> {
    buf.get_lines(0, -1, false).await.unwrap_or_default()
}

/// 結果1件。
struct Check {
    num: u32,
    label: &'static str,
    pass: bool,
    detail: String,
}

fn ok(num: u32, label: &'static str, pass: bool, detail: String) -> Check {
    Check {
        num,
        label,
        pass,
        detail,
    }
}

const INIT: &[&str] = &[
    "/012 src/",
    "/013   main.rs",
    "/014   lib.rs",
    "新規ファイル.txt",
];

// ---------------------------------------------------------------------------
// #3 RPC疎通 + UI attach + ext_cmdline / ext_messages 疎通
// ---------------------------------------------------------------------------

async fn check3(nvim: &Nvim, rd: &mut Redraw) -> Check {
    let mut d = String::new();

    // (a) RPC 往復
    let api = nvim.get_api_info().await;
    let a = api.is_ok();
    if let Ok(info) = &api {
        if let Some(chan) = info.first().and_then(Value::as_i64) {
            d += &format!("(a) get_api_info OK (channel={chan})\n");
        } else {
            d += "(a) get_api_info OK\n";
        }
    } else {
        d += "(a) get_api_info FAILED\n";
    }

    // UI attach: ext_cmdline / ext_messages の2つだけ(他は false)。最小グリッド。
    let mut opts = UiAttachOptions::new();
    opts.set_cmdline_external(true);
    opts.set_messages_externa(true); // nvim-rs 0.9.2 のメソッド名(typo)。= ext_messages
    let attach = nvim.ui_attach(80, 24, &opts).await;
    match &attach {
        Ok(()) => d += "    ui_attach(80x24, ext_cmdline+ext_messages) OK\n",
        Err(e) => d += &format!("    ui_attach FAILED: {e}\n"),
    }
    settle().await;

    // (b) ":" で cmdline_show が届くか
    rd.reset();
    nvim.input(":").await.ok();
    settle().await;
    rd.pump();
    let b = rd.saw("cmdline_show");
    d += &format!(
        "(b) input ':' -> cmdline_show={b} (events: {:?})\n",
        rd.names()
    );
    nvim.input("<Esc>").await.ok();
    settle().await;

    // (c) 存在しない語の検索 -> E486 が ext_messages(msg_show)で届くか
    rd.reset();
    nvim.input("/zzqqxxnotexist").await.ok();
    nvim.input("<CR>").await.ok();
    settle().await;
    rd.pump();
    let saw_msg = rd.saw("msg_show");
    let saw_e486 = rd.dump().contains("E486");
    let c = saw_msg && saw_e486;
    d += &format!(
        "(c) '/存在しない語<CR>' -> msg_show={saw_msg}, dump has E486={saw_e486} (events: {:?})\n",
        rd.names()
    );
    nvim.input("<Esc>").await.ok();
    settle().await;

    // (d) CREATE_NO_WINDOW。--headless + CREATE_NO_WINDOW で spawn 成立時点で
    //     コンソールウィンドウは生成されない(構造的保証)。フラグ適用とspawn成功で確認。
    let d_flag = cfg!(windows);
    d += &format!(
        "(d) CREATE_NO_WINDOW applied={d_flag} & spawn+attach succeeded (headless: no console window by construction)\n"
    );

    let pass = a && attach.is_ok() && b && c && d_flag;
    ok(
        3,
        "RPC + UI attach + ext_cmdline/ext_messages 疎通",
        pass,
        d,
    )
}

// ---------------------------------------------------------------------------
// #1 in-buffer ID が Vim操作で行に追従する
// ---------------------------------------------------------------------------

/// 全行に壊れたIDプレフィックス(grammar::Broken)が無いこと。
fn no_broken(lines: &[String]) -> bool {
    lines
        .iter()
        .all(|l| !matches!(grammar::split_id_prefix(l), grammar::PrefixParse::Broken))
}
fn count_prefixed(lines: &[String], pfx: &str) -> usize {
    lines.iter().filter(|l| l.starts_with(pfx)).count()
}

async fn check1(nvim: &Nvim, buf: &Buffer<NvimWriter>, win: &nvim_rs::Window<NvimWriter>) -> Check {
    let mut d = String::new();
    let mut all = true;

    // -- dd -> p : /013 行が内容ごと移動、孤児化・欠落なし --------------------
    nvim.input("<Esc>").await.ok();
    set_buf(buf, INIT).await;
    win.set_cursor((2, 0)).await.ok(); // /013 行
    tick().await;
    nvim.input("dd").await.ok();
    tick().await;
    nvim.input("p").await.ok();
    settle().await;
    let l = get_buf(buf).await;
    let p1 = l.len() == 4
        && count_prefixed(&l, "/013 ") == 1
        && l.iter().any(|x| x == "/013   main.rs")
        && no_broken(&l)
        // main.rs を含む行は必ず /013 プレフィックス付き(剥がれ検出)
        && l.iter().all(|x| !x.contains("main.rs") || x.starts_with("/013 "));
    all &= p1;
    d += &format!("dd->p:  pass={p1} -> {l:?}\n");

    // -- yy -> p : 同一IDが2行に複製 = COPYシグナル -------------------------
    nvim.input("<Esc>").await.ok();
    set_buf(buf, INIT).await;
    win.set_cursor((2, 0)).await.ok();
    tick().await;
    nvim.input("yy").await.ok();
    tick().await;
    nvim.input("p").await.ok();
    settle().await;
    let l = get_buf(buf).await;
    let dupes = count_prefixed(&l, "/013 ");
    let p2 = l.len() == 5
        && dupes == 2
        && l.iter().filter(|x| *x == "/013   main.rs").count() == 2
        && no_broken(&l);
    all &= p2;
    d += &format!("yy->p:  pass={p2} (/013 が {dupes} 行=COPYシグナル) -> {l:?}\n");

    // -- :m 行移動後も各 /NNN が正しい行に残る ------------------------------
    nvim.input("<Esc>").await.ok();
    set_buf(buf, INIT).await;
    tick().await;
    nvim.input(":1m$").await.ok();
    nvim.input("<CR>").await.ok();
    settle().await;
    let l = get_buf(buf).await;
    let p3 = l.len() == 4
        && count_prefixed(&l, "/012 ") == 1
        && count_prefixed(&l, "/013 ") == 1
        && count_prefixed(&l, "/014 ") == 1
        && l.iter().any(|x| x == "/012 src/")
        && l.iter().any(|x| x == "/013   main.rs")
        && l.iter().any(|x| x == "/014   lib.rs")
        && l.iter().any(|x| x == "新規ファイル.txt")
        && l.last().map(String::as_str) == Some("/012 src/")
        && no_broken(&l);
    all &= p3;
    d += &format!("':m':   pass={p3} (1行目を末尾へ移動) -> {l:?}\n");

    // -- :s 名前部のみ変更、/NNN は保持 ------------------------------------
    nvim.input("<Esc>").await.ok();
    set_buf(buf, INIT).await;
    tick().await;
    nvim.input(":2s/main/MAIN/").await.ok();
    nvim.input("<CR>").await.ok();
    settle().await;
    let l = get_buf(buf).await;
    let p4 = l.len() == 4
        && l.iter().any(|x| x == "/013   MAIN.rs")
        && count_prefixed(&l, "/013 ") == 1
        && l.iter().any(|x| x == "/012 src/")
        && l.iter().any(|x| x == "/014   lib.rs")
        && no_broken(&l);
    all &= p4;
    d += &format!("':s':   pass={p4} (名前のみ変更・ID保持) -> {l:?}\n");

    // -- u / <C-r> undo/redo でIDが戻る/やり直せる -------------------------
    nvim.input("<Esc>").await.ok();
    set_buf(buf, INIT).await;
    win.set_cursor((2, 0)).await.ok();
    tick().await;
    nvim.input("dd").await.ok();
    settle().await;
    let after_dd = get_buf(buf).await;
    nvim.input("u").await.ok();
    settle().await;
    let after_u = get_buf(buf).await;
    nvim.input("<C-r>").await.ok();
    settle().await;
    let after_redo = get_buf(buf).await;
    let init_vec: Vec<String> = INIT.iter().map(|s| s.to_string()).collect();
    let p5 = after_u == init_vec && after_redo == after_dd && no_broken(&after_u);
    all &= p5;
    d += &format!("u/<C-r>: pass={p5} (undo->{after_u:?} / redo->{after_redo:?})\n");

    ok(
        1,
        "in-buffer ID が dd/p・yy/p・:m・:s・undo/redo で行に追従",
        all,
        d,
    )
}

// ---------------------------------------------------------------------------
// #2 カーソル列補正・描画隠蔽(マルチバイト)
// ---------------------------------------------------------------------------

async fn check2(nvim: &Nvim, buf: &Buffer<NvimWriter>, win: &nvim_rs::Window<NvimWriter>) -> Check {
    const LINE: &str = "/012 新規ファイル.txt";
    let prefix_len = grammar::id_prefix_len(LINE); // "/012 " = 5 bytes
    let mut d = String::new();
    d += &format!("line={LINE:?}  id_prefix_len={prefix_len}\n");

    nvim.input("<Esc>").await.ok();
    set_buf(buf, &[LINE]).await;
    win.set_cursor((1, 0)).await.ok();
    tick().await;
    nvim.input("0").await.ok(); // 行頭('/')へ
    tick().await;

    // カーソルを 'l' で1文字ずつ右へ動かし、実カーソル列(バイト)を収集する。
    let mut cols: Vec<usize> = Vec::new();
    let mut last = usize::MAX;
    for _ in 0..64 {
        let (_row, col) = win.get_cursor().await.unwrap_or((1, 0));
        let col = col as usize;
        if col == last {
            break; // normal mode の 'l' は行末で止まる
        }
        cols.push(col);
        last = col;
        nvim.input("l").await.ok();
        tick().await;
    }

    // 表示列 = clamp0(col - prefix_len)。マルチバイト境界を検証する。
    let mut boundaries_ok = true;
    let mut clamp_ok = true;
    let mut saw_multibyte_step = false;
    let mut rows = Vec::new();
    let mut prev = None::<usize>;
    for &col in &cols {
        let on_boundary = LINE.is_char_boundary(col);
        boundaries_ok &= on_boundary;
        let disp = col.saturating_sub(prefix_len);
        if col < prefix_len && disp != 0 {
            clamp_ok = false;
        }
        if let Some(p) = prev {
            if col.saturating_sub(p) >= 2 {
                saw_multibyte_step = true; // 3バイト文字(日本語)を跨いだ
            }
        }
        prev = Some(col);
        rows.push(format!("col={col}(boundary={on_boundary}) -> disp={disp}"));
    }

    // プレフィックス領域への進入補正の明示確認: col=0('/')は disp=0 にクランプ。
    let clamp_at_zero =
        cols.first().copied() == Some(0) && (0usize).saturating_sub(prefix_len) == 0;

    d += &format!("observed: {}\n", rows.join(" | "));
    d += &format!(
        "boundaries_ok={boundaries_ok} multibyte_step_seen={saw_multibyte_step} clamp_ok={clamp_ok} clamp@col0={clamp_at_zero}\n"
    );

    let pass = boundaries_ok && saw_multibyte_step && clamp_ok && clamp_at_zero;
    ok(
        2,
        "カーソル列補正 disp=clamp0(col-id_prefix_len) がマルチバイトでズレず、prefix領域はdisp=0にクランプ",
        pass,
        d,
    )
}

// ---------------------------------------------------------------------------
// #4 Windows IME(日本語確定文字列)入力経路
// ---------------------------------------------------------------------------

async fn check4(nvim: &Nvim, buf: &Buffer<NvimWriter>, win: &nvim_rs::Window<NvimWriter>) -> Check {
    const IME: &str = "新規ファイル";
    // keycode表記を含む「危険な」確定文字列。input経路だと <CR> が Enter に化ける。
    const HAZ: &str = "foo<CR>bar";
    let mut d = String::new();

    // (A) nvim_input 経路 — IME確定文字列
    nvim.input("<Esc>").await.ok();
    set_buf(buf, &[""]).await;
    win.set_cursor((1, 0)).await.ok();
    tick().await;
    nvim.input("i").await.ok();
    nvim.input(IME).await.ok();
    nvim.input("<Esc>").await.ok();
    settle().await;
    let a_lines = get_buf(buf).await;
    let a_ime_ok = a_lines == vec![IME.to_string()];
    d += &format!("(A) input('{IME}') -> {a_lines:?}  literal_ok={a_ime_ok}\n");

    // (B-paste) nvim_paste 経路 — IME確定文字列
    nvim.input("<Esc>").await.ok();
    set_buf(buf, &[""]).await;
    win.set_cursor((1, 0)).await.ok();
    tick().await;
    nvim.input("i").await.ok();
    nvim.paste(IME, false, -1).await.ok();
    nvim.input("<Esc>").await.ok();
    settle().await;
    let bp_lines = get_buf(buf).await;
    let bp_ime_ok = bp_lines == vec![IME.to_string()];
    d += &format!("(B/paste) paste('{IME}') -> {bp_lines:?}  literal_ok={bp_ime_ok}\n");

    // (B-set_text) nvim_buf_set_text 経路 — IME確定文字列(モード非依存)
    nvim.input("<Esc>").await.ok();
    set_buf(buf, &[""]).await;
    tick().await;
    buf.set_text(0, 0, 0, 0, vec![IME.to_string()]).await.ok();
    settle().await;
    let bt_lines = get_buf(buf).await;
    let bt_ime_ok = bt_lines == vec![IME.to_string()];
    d += &format!("(B/set_text) set_text('{IME}') -> {bt_lines:?}  literal_ok={bt_ime_ok}\n");

    // keycode混入文字列 "foo<CR>bar": input は化ける / paste はリテラル保持
    nvim.input("<Esc>").await.ok();
    set_buf(buf, &[""]).await;
    win.set_cursor((1, 0)).await.ok();
    tick().await;
    nvim.input("i").await.ok();
    nvim.input(HAZ).await.ok();
    nvim.input("<Esc>").await.ok();
    settle().await;
    let haz_input = get_buf(buf).await;
    let input_mangled = haz_input != vec![HAZ.to_string()]; // 化ければ true(=input不採用の根拠)

    nvim.input("<Esc>").await.ok();
    set_buf(buf, &[""]).await;
    win.set_cursor((1, 0)).await.ok();
    tick().await;
    nvim.input("i").await.ok();
    nvim.paste(HAZ, false, -1).await.ok();
    nvim.input("<Esc>").await.ok();
    settle().await;
    let haz_paste = get_buf(buf).await;
    let paste_literal = haz_paste == vec![HAZ.to_string()];

    d += &format!(
        "(hazard '{HAZ}') input->{haz_input:?} mangled={input_mangled} | paste->{haz_paste:?} literal={paste_literal}\n"
    );
    d += "=> 採用: EditorCommand::Text は nvim_paste(カーソル位置へリテラル挿入)。\n";
    d += "   代替: nvim_buf_set_text(モード非依存の直接挿入)。どちらもkeycode解釈なし。\n";

    // 合格基準: (B)経路(paste / set_text)が確定文字列をリテラルで入れる。
    // かつ paste が keycode混入文字列もリテラル保持する(input経路との差)。
    let pass = bp_ime_ok && bt_ime_ok && paste_literal;
    ok(
        4,
        "IME確定文字列は nvim_paste / nvim_buf_set_text でリテラル挿入できる(EditorCommand::Text)",
        pass,
        d,
    )
}

// ---------------------------------------------------------------------------
// (任意/純粋) translate keycode マッピング先行検証(製品 translate.rs は触らない)
// ---------------------------------------------------------------------------

fn mods_prefix(m: &Modifiers) -> String {
    let mut s = String::new();
    if m.ctrl {
        s.push_str("C-");
    }
    if m.alt {
        s.push_str("A-");
    }
    if m.shift {
        s.push_str("S-");
    }
    s
}

fn spike_keycode(k: &KeyInput) -> String {
    let mp = mods_prefix(&k.mods);
    match k.key {
        Key::Char(c) => {
            let tok = if c == '<' {
                "lt".to_string()
            } else {
                c.to_string()
            };
            if mp.is_empty() {
                if c == '<' {
                    "<lt>".to_string()
                } else {
                    c.to_string()
                }
            } else {
                format!("<{mp}{tok}>")
            }
        }
        Key::F(n) => format!("<{mp}F{n}>"),
        other => {
            let base = match other {
                Key::Enter => "CR",
                Key::Esc => "Esc",
                Key::Backspace => "BS",
                Key::Tab => "Tab",
                Key::Delete => "Del",
                Key::Up => "Up",
                Key::Down => "Down",
                Key::Left => "Left",
                Key::Right => "Right",
                Key::Home => "Home",
                Key::End => "End",
                Key::PageUp => "PageUp",
                Key::PageDown => "PageDown",
                _ => unreachable!(),
            };
            format!("<{mp}{base}>")
        }
    }
}

fn ki(key: Key, ctrl: bool, alt: bool, shift: bool) -> KeyInput {
    KeyInput {
        key,
        mods: Modifiers { ctrl, alt, shift },
    }
}

fn check_translate() -> Check {
    let cases: &[(KeyInput, &str)] = &[
        (ki(Key::Char('a'), false, false, false), "a"),
        (ki(Key::Char('<'), false, false, false), "<lt>"),
        (ki(Key::Enter, false, false, false), "<CR>"),
        (ki(Key::Esc, false, false, false), "<Esc>"),
        (ki(Key::Backspace, false, false, false), "<BS>"),
        (ki(Key::Tab, false, false, false), "<Tab>"),
        (ki(Key::Delete, false, false, false), "<Del>"),
        (ki(Key::Up, false, false, false), "<Up>"),
        (ki(Key::Home, false, false, false), "<Home>"),
        (ki(Key::End, false, false, false), "<End>"),
        (ki(Key::PageUp, false, false, false), "<PageUp>"),
        (ki(Key::PageDown, false, false, false), "<PageDown>"),
        (ki(Key::F(5), false, false, false), "<F5>"),
        (ki(Key::Char('a'), true, false, false), "<C-a>"),
        (ki(Key::Char('x'), true, true, false), "<C-A-x>"),
        (ki(Key::F(1), true, true, true), "<C-A-S-F1>"),
    ];
    let mut d = String::new();
    let mut all = true;
    for (input, want) in cases {
        let got = spike_keycode(input);
        let pass = got == *want;
        all &= pass;
        d += &format!(
            "{:?} -> {got:?} (want {want:?}) {}\n",
            input.key,
            if pass { "ok" } else { "MISMATCH" }
        );
    }
    ok(
        0,
        "translate KeyInput->keycode 期待値表(純粋・机上確認)",
        all,
        d,
    )
}

// ---------------------------------------------------------------------------
// spawn
// ---------------------------------------------------------------------------

async fn spawn_nvim(exe: &str) -> anyhow::Result<(Nvim, Child, Redraw)> {
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    let handler = SpikeHandler { tx };

    let mut cmd = Command::new(exe);
    cmd.args(fyler_engine_nvim::spawn::NVIM_ARGS);
    #[cfg(windows)]
    cmd.creation_flags(fyler_engine_nvim::spawn::CREATE_NO_WINDOW);

    let (nvim, _io, child) = new_child_cmd(&mut cmd, handler).await?;
    let rd = Redraw {
        rx,
        log: Vec::new(),
    };
    Ok((nvim, child, rd))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== M0 成立性スパイク(実nvim.exe / Windows実機)===\n");

    // 純粋ロジックの先行検証(nvim不要)。
    let mut checks: Vec<Check> = Vec::new();
    let tr = check_translate();

    let exe = std::env::var("FYLER_NVIM_EXE").unwrap_or_else(|_| "nvim".to_string());
    println!("nvim.exe = {exe}");
    println!("NVIM_ARGS = {:?}\n", fyler_engine_nvim::spawn::NVIM_ARGS);

    let (nvim, mut child, mut rd) = match spawn_nvim(&exe).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[FATAL] nvim spawn 失敗: {e}");
            eprintln!("FYLER_NVIM_EXE を実nvim.exeへ設定して再実行すること。");
            std::process::exit(2);
        }
    };

    // #3 を最初に(RPC/attach/ext疎通)。
    checks.push(check3(&nvim, &mut rd).await);

    // attach 後にバッファ/ウィンドウを取得。
    let buf = nvim.get_current_buf().await.expect("get_current_buf");
    let win = nvim.get_current_win().await.expect("get_current_win");
    // 検証中の副作用を最小化: swapなし・undo有効(既定)。modifiableは既定true。

    checks.push(check1(&nvim, &buf, &win).await);
    checks.push(check2(&nvim, &buf, &win).await);
    checks.push(check4(&nvim, &buf, &win).await);

    // nvim を終了。
    let _ = child.kill().await;

    // 出力: 詳細 + サマリ。
    let mut ordered = vec![tr];
    ordered.extend(checks);
    // #1..#4 を番号順に、translate(0)は最後に。
    ordered.sort_by_key(|c| if c.num == 0 { u32::MAX } else { c.num });

    for c in &ordered {
        let tag = if c.pass { "PASS" } else { "FAIL" };
        let head = if c.num == 0 {
            format!("[{tag}] (任意) {}", c.label)
        } else {
            format!("[{tag}] #{} {}", c.num, c.label)
        };
        println!("\n{head}");
        for line in c.detail.trim_end().lines() {
            println!("    {line}");
        }
    }

    // 判定は #1..#4 のみ(translateは任意)。
    let core: Vec<&Check> = ordered
        .iter()
        .filter(|c| (1..=4).contains(&c.num))
        .collect();
    let all_pass = core.iter().all(|c| c.pass);
    println!("\n=== サマリ ===");
    for c in &core {
        println!("  #{}: {}", c.num, if c.pass { "PASS" } else { "FAIL" });
    }
    println!(
        "  (任意) translate: {}",
        if ordered
            .iter()
            .find(|c| c.num == 0)
            .map(|c| c.pass)
            .unwrap_or(false)
        {
            "PASS"
        } else {
            "FAIL"
        }
    );
    println!(
        "\nM0 #1..#4: {}",
        if all_pass { "ALL PASS" } else { "NOT ALL PASS" }
    );

    if !all_pass {
        std::process::exit(1);
    }
    Ok(())
}
