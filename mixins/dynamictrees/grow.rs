// Runs before the game ticks blocks: raises Dynamic Trees' staged trees.
// Compiled inside horizon_client::game, so bare names from that module are in scope.
fn dt_grow(game: &mut crate::game::Game<'_>) {
    crate::game::dynamictrees::grow_tick(game);
}
