//! Serve one fake agent over stdio, standalone: the same launcher `nori agent serve fake` uses,
//! without the rest of Nori.
//!
//!     cargo run -p nori-fake-agent --example serve -- world=fake-agent/worlds/smoke.pkl [agent=<slug>] [clock=x10]
//!
//! Nothing but ACP goes to stdout. The clock is real unless `clock=` says otherwise.

use nori_acp_kit::Host;
use nori_acp_kit::InProcessSpec;
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let spec = InProcessSpec {
        agent: nori_fake_agent::NAME.to_owned(),
        home: None,
        config: std::env::args().skip(1).collect(),
        environment: Default::default(),
    };
    let registration = nori_fake_agent::registration();
    let (host_end, agent_end) = tokio::io::duplex(1 << 16);
    let (mut from_agent, mut to_agent) = tokio::io::split(host_end);
    let inbound = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let _ = tokio::io::copy(&mut stdin, &mut to_agent).await;
        let _ = to_agent.shutdown().await;
    });
    let outbound = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let _ = tokio::io::copy(&mut from_agent, &mut stdout).await;
        stdout.flush().await
    });
    let served = (registration.launch)(spec, agent_end, Host::External).await;
    inbound.abort();
    let _ = outbound.await;
    served
}
