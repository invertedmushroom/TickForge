use rapier3d::parry::query;
use rapier3d::parry::shape::{Ball, Capsule};
use rapier3d::parry::math::Isometry;
fn test() {
    let iso1 = Isometry::translation(0.0, 0.0, 0.0);
    let s1 = Ball::new(1.0);
    let s2 = Capsule::new_y(0.5, 0.3);
    let _ = query::intersection_test(&iso1, &s1, &iso1, &s2);
}
