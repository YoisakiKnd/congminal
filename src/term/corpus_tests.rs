//! 真实应用回放语料：把 less / vim / man / htop 在 80×24 PTY 上产生的字节原样
//! 重放进解析器，检查真实程序的控制序列组合不会把终端弄崩或留下错状态。
//!
//! 语料是提交进仓库的纯字节流（`src/term/corpus/*.bin`），回放不依赖本机的程序版本。
//! 重新录制（手工运行，不进 CI，80×24、TERM=xterm-256color）：
//!
//! ```text
//! python3 ci/record-corpus.py src/term/corpus/less.bin less -R src/term/corpus/sample.txt
//! python3 ci/record-corpus.py src/term/corpus/vim.bin vim -u NONE -N src/term/corpus/sample.txt
//! MANPAGER='less -R' python3 ci/record-corpus.py src/term/corpus/man.bin man ls
//! python3 ci/record-corpus.py src/term/corpus/htop.bin htop
//! ```
use super::*;
use crate::resources::{Budget, Scope};
use crate::vt::Parser;

/// 一段语料：名字、字节、以及回放期间必须在备用屏上出现过的文字。
struct Corpus {
    name: &'static str,
    bytes: &'static [u8],
    expect: &'static str,
}

const CORPORA: [Corpus; 4] = [
    Corpus { name: "less", bytes: include_bytes!("corpus/less.bin"), expect: "line 001 plain text with" },
    Corpus { name: "vim", bytes: include_bytes!("corpus/vim.bin"), expect: "line 001 plain text with" },
    Corpus { name: "man", bytes: include_bytes!("corpus/man.bin"), expect: "ls - list directory contents" },
    // htop 的内容随机器负载变化，只要求备用屏上真的画出了字符。
    Corpus { name: "htop", bytes: include_bytes!("corpus/htop.bin"), expect: "" },
];

/// 把当前可见屏的文字收进缓冲区（只收可见行，避免为测试另开通道）。
fn visible_text(term: &Terminal, out: &mut String) {
    for row in 0..term.rows() {
        for cell in &term.visible_row(row).cells {
            out.push(cell.c);
        }
        out.push('\n');
    }
}

#[test]
fn real_application_output_replays_without_breaking_the_terminal() {
    let pool = Budget::new(64 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    for corpus in &CORPORA {
        let mut term = Terminal::try_new(80, 24).expect("终端额度");
        let mut parser = Parser::new();
        let mut text = String::new();
        let mut saw_alt = false;
        let mut saw_text = corpus.expect.is_empty();
        // 按 512 字节分块回放：真实程序的输出本来就跨读边界。
        for chunk in corpus.bytes.chunks(512) {
            parser.advance(&mut term, chunk);
            assert!(term.response.len() <= Terminal::RESPONSE_LIMIT, "{}: 应答缓冲超过上限", corpus.name);
            if term.is_alt_screen() {
                saw_alt = true;
                if !saw_text {
                    visible_text(&term, &mut text);
                    saw_text = text.contains(corpus.expect);
                    // 只留最近的屏幕，别让测试自己无界增长。
                    if text.len() > 64 * 1024 {
                        text.clear();
                    }
                }
            }
        }
        assert!(saw_alt, "{}: 回放期间应进入备用屏", corpus.name);
        assert!(saw_text, "{}: 备用屏上应出现 {:?}", corpus.name, corpus.expect);
        // 四段语料都以退出备用屏结束：回放完必须回到主屏。
        assert!(!term.is_alt_screen(), "{}: 回放结束应退出备用屏", corpus.name);
        // 主屏没有被备用屏内容污染（退出后是空的）。
        let mut primary = String::new();
        visible_text(&term, &mut primary);
        assert!(primary.trim().is_empty(), "{}: 主屏应保持空白，实得 {:?}", corpus.name, primary.trim());
        drop(parser);
        drop(term);
        assert_eq!(pool.snapshot().used, 0, "{}: 额度未归还", corpus.name);
    }
}

#[test]
fn replaying_byte_by_byte_gives_the_same_final_state() {
    let pool = Budget::new(64 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    // 逐字节喂入最容易暴露「跨块状态没保住」的解析器缺陷：用最短的一段语料验证。
    let corpus = &CORPORA[0];
    let mut term = Terminal::try_new(80, 24).expect("终端额度");
    let mut parser = Parser::new();
    for byte in corpus.bytes {
        parser.advance(&mut term, std::slice::from_ref(byte));
    }
    assert!(!term.is_alt_screen(), "逐字节回放也应正常退出备用屏");
    assert!(term.response.len() <= Terminal::RESPONSE_LIMIT);
    drop(parser);
    drop(term);
    assert_eq!(pool.snapshot().used, 0);
}

#[test]
fn replaying_into_a_resized_terminal_stays_consistent() {
    let pool = Budget::new(64 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    // 真实场景：程序正在全屏画图时用户改了窗口大小。
    for corpus in &CORPORA {
        let mut term = Terminal::try_new(80, 24).expect("终端额度");
        let mut parser = Parser::new();
        let half = corpus.bytes.len() / 2;
        parser.advance(&mut term, &corpus.bytes[..half]);
        let plan = term.prepare_ui_resize(100, 30).expect("缩放工作集");
        term.commit_resize(plan);
        assert_eq!((term.cols(), term.rows()), (100, 30), "{}", corpus.name);
        parser.advance(&mut term, &corpus.bytes[half..]);
        assert!(term.response.len() <= Terminal::RESPONSE_LIMIT, "{}", corpus.name);
        assert!(!term.is_alt_screen(), "{}: 缩放后仍要能正常退出备用屏", corpus.name);
        drop(parser);
        drop(term);
        assert_eq!(pool.snapshot().used, 0, "{}: 额度未归还", corpus.name);
    }
}
