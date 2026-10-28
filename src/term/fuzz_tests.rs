//! 解析器与终端的健壮性回归：用确定性伪随机字节流反复喂入，检查不 panic、
//! 应答缓冲有上限、额度随会话释放。
//!
//! 这不是覆盖率导向的模糊测试器（不引入 `cargo-fuzz`），而是把「任意字节序列都不该
//! 让终端崩掉或无限吃内存」这条不变量钉进 CI：种子固定，失败可复现。
use super::*;
use crate::resources::{Budget, Scope};
use crate::vt::Parser;

/// 确定性伪随机源（xorshift64*）：固定种子，失败可复现。
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 { 0 } else { (self.next() % bound as u64) as usize }
    }
}

/// 按权重挑一个片段：控制序列、UTF-8 片段、可打印文本与裸字节混在一起。
fn fragment(rng: &mut Rng, out: &mut Vec<u8>) {
    const PIECES: [&[u8]; 24] = [
        b"\x1b[",
        b"\x1b]",
        b"\x1bP",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1b[?2027h",
        b"\x1b[38;2;1;2;3m",
        b"\x1b[38:2::1:2:3m",
        b"\x1b[4:3m",
        b"\x1b[?25l",
        b"\x1b[2J",
        b"\x1b[999;999H",
        b"\x1b[?u",
        b"\x1b[>1u",
        b"\x1b]8;;https://example.com\x07",
        b"\x1b]133;A\x07",
        b"\x1b]4;1;?\x07",
        b"\x1b]10;?\x07",
        b"\x1bP$qm\x1b\\",
        b"\x1b[14t",
        b"\x1b[18t",
        b"\x1b[?2026h",
        b"\x1b[?2026l",
        b"\x1b[1;1r",
    ];
    match rng.below(10) {
        0..=4 => {
            let piece = PIECES[rng.below(PIECES.len())];
            out.extend_from_slice(piece);
            // 控制序列后面随机跟参数，制造不完整与超长序列。
            for _ in 0..rng.below(6) {
                const PARAMS: &[u8] = b"0123456789;:?$ \"'";
                out.push(PARAMS[rng.below(PARAMS.len())]);
            }
        }
        5..=6 => {
            // 合法与非法 UTF-8 混着来：截断的多字节、超长编码、孤立续字节。
            let choices: [&[u8]; 6] =
                ["中".as_bytes(), "🙂".as_bytes(), "e\u{301}".as_bytes(), &[0xf0, 0x9f], &[0x80], &[0xc0, 0xaf]];
            out.extend_from_slice(choices[rng.below(choices.len())]);
        }
        7..=8 => {
            let count = 1 + rng.below(24);
            for _ in 0..count {
                const TEXT: &[u8] = b" \t\r\nabcXYZ019-_/\\|";
                out.push(TEXT[rng.below(TEXT.len())]);
            }
        }
        _ => out.push((rng.next() & 0xff) as u8),
    }
}

#[test]
fn random_byte_streams_never_panic_and_keep_the_response_buffer_bounded() {
    let pool = Budget::new(64 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    for seed in 1..=8u64 {
        let mut rng = Rng::new(seed);
        let mut term = Terminal::try_new(80, 24).expect("终端额度");
        let mut parser = Parser::new();
        let mut bytes = Vec::new();
        for round in 0..64 {
            bytes.clear();
            for _ in 0..rng.below(64) + 1 {
                fragment(&mut rng, &mut bytes);
            }
            // 分块喂入：解析器必须能跨块保持状态。
            let mut offset = 0;
            while offset < bytes.len() {
                let end = (offset + 1 + rng.below(32)).min(bytes.len());
                parser.advance(&mut term, &bytes[offset..end]);
                offset = end;
                assert!(term.response.len() <= Terminal::RESPONSE_LIMIT, "应答缓冲超过上限：seed {seed} round {round}");
            }
            // 中途缩放：重排与备用屏切换都不能崩。
            // 缩放要先按会话的做法预留工作集（`prepare_ui_resize`），否则 `resize`
            // 会按“未准入”拒绝——这条不变量本身也是被测行为。
            if round % 7 == 3 {
                let cols = 20 + rng.below(120);
                let rows = 5 + rng.below(60);
                let plan = term.prepare_ui_resize(cols, rows);
                assert!(plan.is_ok(), "缩放工作集预留失败：seed {seed} round {round}");
                // 与会话完全相同的两步：先准入，再用同一份计划提交。
                term.commit_resize(plan.unwrap());
                assert_eq!(term.cols(), cols);
                assert_eq!(term.rows(), rows);
            }
        }
        // 会话结束时额度全部归还：解析器与终端都不该留下租约。
        drop(parser);
        drop(term);
        assert_eq!(pool.snapshot().used, 0, "seed {seed} 结束后额度未归还");
    }
}

/// 备用屏激活、历史超过同步重排上限、高度变矮：被推入历史的是备用屏顶部行，
/// 预留必须按保存的主屏光标算，否则 `History::reserve` 会因为工作集不够而 panic。
#[test]
fn shrinking_the_height_while_the_alt_screen_is_active_reserves_the_pushed_rows() {
    let pool = Budget::new(64 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    let mut term = Terminal::try_new(80, 24).expect("终端额度");
    let mut parser = Parser::new();
    // 先攒出超过 SYNC_REFLOW_LIMIT 的历史。
    for line in 0..600 {
        parser.advance(&mut term, format!("line {line} ........................................\r\n").as_bytes());
    }
    assert!(
        term.history().used_bytes() + term.grid().allocated_bytes() > SYNC_REFLOW_LIMIT,
        "历史要超过同步重排上限：{}",
        term.history().used_bytes()
    );
    // 进备用屏并把光标留在底部，然后只减高度（宽度不变）。
    parser.advance(&mut term, b"\x1b[?1049h\x1b[24;1Halt screen");
    assert!(term.is_alt_screen());
    let plan = term.prepare_ui_resize(80, 10).expect("缩放工作集预留");
    term.commit_resize(plan);
    assert_eq!(term.rows(), 10);
    assert!(term.is_alt_screen());
    // 退出备用屏后主屏内容仍在，且额度全部归还。
    parser.advance(&mut term, b"\x1b[?1049l");
    drop(parser);
    drop(term);
    assert_eq!(pool.snapshot().used, 0);
}

#[test]
fn a_single_hostile_sequence_cannot_grow_state_without_bound() {
    let pool = Budget::new(64 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    let mut term = Terminal::try_new(80, 24).expect("终端额度");
    let mut parser = Parser::new();
    // 超长 OSC（无终止符）、超长 CSI 参数、超长 DCS：都只该被丢弃，不该累积。
    for _ in 0..64 {
        parser.advance(&mut term, b"\x1b]0;");
        parser.advance(&mut term, &vec![b'x'; 4096]);
        parser.advance(&mut term, b"\x1b[");
        parser.advance(&mut term, &vec![b'9'; 4096]);
        parser.advance(&mut term, b"\x1bP");
        parser.advance(&mut term, &vec![b'q'; 4096]);
    }
    assert!(term.response.len() <= Terminal::RESPONSE_LIMIT);
    // 收尾：一个完整序列仍能正常工作（解析器没有卡在坏状态里）。
    parser.advance(&mut term, b"\x1b[2J\x1b[Hhi");
    let text: String = term.visible_row(0).cells.iter().map(|cell| cell.c).collect();
    assert_eq!(text.trim_end(), "hi");
    drop(parser);
    drop(term);
    assert_eq!(pool.snapshot().used, 0);
}
