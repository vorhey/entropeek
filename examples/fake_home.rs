//! Creates a fake home directory full of agent sessions, for trying
//! entropeek safely and for load testing.
//!
//!     cargo run --release --example fake_home -- /tmp/fake            # small, varied set
//!     cargo run --release --example fake_home -- /tmp/big 5000 20     # 5000 sessions x 20 turns
//!
//! Then point entropeek at it:
//!
//!     HOME=/tmp/fake CLAUDE_CONFIG_DIR=/tmp/fake/.claude CODEX_HOME=/tmp/fake/.codex \
//!       ENTROPEEK_CACHE_DIR=/tmp/fake/.cache ENTROPEEK_DATA_DIR=/tmp/fake/.data entropeek

use std::path::PathBuf;

use entropeek::env::Env;
use entropeek::testkit;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("usage: fake_home <dir> [sessions] [turns]"));
    let env = Env::under(&root);
    match args.next() {
        Some(count) => {
            let count: usize = count.parse().expect("sessions must be a number");
            let turns: usize = args.next().map_or(10, |t| t.parse().expect("turns must be a number"));
            testkit::synthetic(&env, count, turns);
            println!("wrote {count} synthetic sessions ({turns} turns each) under {}", root.display());
        }
        None => {
            let ids = testkit::populate(&env);
            println!("wrote {} sessions under {}", ids.len(), root.display());
        }
    }
}
