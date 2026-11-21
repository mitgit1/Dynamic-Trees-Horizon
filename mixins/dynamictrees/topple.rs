// Runs before the game breaks the clicked block. A trunk with leaves above
// falls whole (Break with the work already done); anything else lets the
// original break run (Continue).
fn dt_topple(game: &mut crate::game::Game<'_>, pos: [i32; 3]) -> core::ops::ControlFlow<Result<()>> {
    match crate::game::dynamictrees::topple(game, pos) {
        Some(result) => core::ops::ControlFlow::Break(result),
        None => core::ops::ControlFlow::Continue(()),
    }
}
