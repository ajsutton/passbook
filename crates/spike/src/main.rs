//! Spike: proves L1 + OP facades co-resolve in one workspace and that one
//! generic ExEx fn compiles for both EthereumNode and OpNode.
use reth_ethereum::{
    exex::{ExExContext, ExExEvent},
    node::{api::FullNodeComponents, EthereumNode},
};
use futures::TryStreamExt;

async fn exex<Node: FullNodeComponents>(mut ctx: ExExContext<Node>) -> eyre::Result<()> {
    while let Some(n) = ctx.notifications.try_next().await? {
        if let Some(c) = n.committed_chain() {
            ctx.events.send(ExExEvent::FinishedHeight(c.tip().num_hash()))?;
        }
    }
    Ok(())
}

fn main() -> eyre::Result<()> {
    // Compile-only: reference both node types from the two facades.
    let _ = EthereumNode::default();
    let _ = reth_op::node::OpNode::default();
    Ok(())
}
