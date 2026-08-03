use super::*;

/// Prevent a uniquely owned committed-world history spine from recursively
/// consuming the dropping thread's stack. Shared ancestors are left alone;
/// whichever owner eventually becomes last will resume the same iterative
/// dismantling from that node.
impl Drop for HistoryNode {
    fn drop(&mut self) {
        let mut parent = self.parent.take();
        while let Some(node) = parent {
            match Arc::try_unwrap(node) {
                Ok(mut owned) => {
                    parent = owned.parent.take();
                    // `owned` now drops with no parent, so its Drop is constant-depth.
                }
                Err(shared) => {
                    // Another world still owns this ancestor. Releasing our one
                    // reference cannot destroy it, so there is no recursive tail.
                    drop(shared);
                    break;
                }
            }
        }
    }
}
