//! Chest-cart movement (1.21.1 cot/cou). World collision and inventory remain
//! with their existing owners; this module only advances a supplied state.
type Pos = (i32, i32, i32);
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct State {
    pub pos: [f64; 3],
    pub velocity: [f64; 3],
    pub on_ground: bool,
    pub in_water: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape { NorthSouth, EastWest, AscendingEast, AscendingWest, AscendingNorth, AscendingSouth, SouthEast, SouthWest, NorthWest, NorthEast }
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rail { pub shape: Shape, pub powered: Option<bool> }
impl Shape {
    fn exits(self) -> [[i32;3];2] {
        use Shape::*;
        match self {
            NorthSouth=>[[0,0,-1],[0,0,1]], EastWest=>[[-1,0,0],[1,0,0]],
            AscendingEast=>[[-1,-1,0],[1,0,0]], AscendingWest=>[[-1,0,0],[1,-1,0]],
            AscendingNorth=>[[0,0,-1],[0,-1,1]], AscendingSouth=>[[0,-1,-1],[0,0,1]],
            SouthEast=>[[0,0,1],[1,0,0]], SouthWest=>[[0,0,1],[-1,0,0]],
            NorthWest=>[[0,0,-1],[-1,0,0]], NorthEast=>[[0,0,-1],[1,0,0]],
        }
    }
}
pub(crate) trait World {
    fn rail(&mut self, at: Pos) -> Option<Rail>;
    /// Collision-clipped displacement and the resulting on-ground flag.
    fn move_cart(&mut self, pos: [f64;3], delta: [f64;3]) -> ([f64;3], bool);
    fn conductor(&mut self, at: Pos) -> bool;
    /// Powered activator rails (`minecraft:activator_rail`) eject every entity
    /// riding them each tick. The world owns both the block lookup and the
    /// passenger list, so it decides whether `at` is a powered activator rail
    /// and returns how many riders were ejected. Plain, detector and unpowered
    /// activator rails eject nothing; the default reports zero.
    fn activator_eject(&mut self, _at: Pos) -> usize { 0 }
}
fn horizontal_speed(v: [f64;3]) -> f64 { (v[0]*v[0]+v[2]*v[2]).sqrt() }
fn block(p: [f64;3]) -> Pos { (p[0].floor() as i32,p[1].floor() as i32,p[2].floor() as i32) }
fn rail_at(w: &mut impl World,p: [f64;3]) -> Option<(Pos,Rail)> {
    let (x,mut y,z)=block(p);
    if w.rail((x,y-1,z)).is_some() { y-=1; }
    w.rail((x,y,z)).map(|rail| ((x,y,z),rail))
}
fn project(w: &mut impl World,p:[f64;3])->Option<[f64;3]> {
    let ((x,y,z),rail)=rail_at(w,p)?;
    let [a,b]=rail.shape.exits();
    let start=[x as f64+0.5+a[0] as f64*0.5,y as f64+0.0625+a[1] as f64*0.5,z as f64+0.5+a[2] as f64*0.5];
    let end=[x as f64+0.5+b[0] as f64*0.5,y as f64+0.0625+b[1] as f64*0.5,z as f64+0.5+b[2] as f64*0.5];
    let dx=end[0]-start[0];let dy=(end[1]-start[1])*2.;let dz=end[2]-start[2];
    let t=if dx==0. {p[2]-z as f64} else if dz==0. {p[0]-x as f64} else {(p[0]-start[0])*dx*2.+(p[2]-start[2])*dz*2.};
    let y=start[1]+dy*t+if dy<0. {1.} else if dy>0. {0.5} else {0.};
    Some([start[0]+dx*t,y,start[2]+dz*t])
}
impl State {
    fn move_by(&mut self,w:&mut impl World,delta:[f64;3]) {
        let (actual,ground)=w.move_cart(self.pos,delta);
        for i in 0..3 { self.pos[i]+=actual[i];if actual[i]!=delta[i] {self.velocity[i]=0.;} }
        self.on_ground=ground;
    }
    /// `cou.applyNaturalSlowdown` uses a *float* comparator-derived coefficient.
    pub(crate) fn tick(&mut self,w:&mut impl World,loot_pending:bool,signal:i32) {
        self.velocity[1]-=if self.in_water {0.005} else {0.04};
        let max=if self.in_water {0.2} else {0.4};
        let Some(((x,y,z),rail))=rail_at(w,self.pos) else {
            self.velocity[0]=self.velocity[0].clamp(-max,max);
            self.velocity[2]=self.velocity[2].clamp(-max,max);
            if self.on_ground {for v in &mut self.velocity {*v*=0.5;}}
            self.move_by(w,self.velocity);
            if !self.on_ground {for v in &mut self.velocity {*v*=0.95;}}
            return;
        };
        let before=project(w,self.pos);
        let [a,b]=rail.shape.exits();
        let mut py=y as f64;
        let slope=if self.in_water {0.0078125*0.2} else {0.0078125};
        use Shape::*;
        match rail.shape {
            AscendingEast=>{self.velocity[0]-=slope;py+=1.;},
            AscendingWest=>{self.velocity[0]+=slope;py+=1.;},
            AscendingNorth=>{self.velocity[2]+=slope;py+=1.;},
            AscendingSouth=>{self.velocity[2]-=slope;py+=1.;}, _=>{}
        }
        let mut dx=(b[0]-a[0]) as f64;let mut dz=(b[2]-a[2]) as f64;
        let length=(dx*dx+dz*dz).sqrt();
        if self.velocity[0]*dx+self.velocity[2]*dz<0. {dx = -dx;dz = -dz;}
        let speed=2f64.min(horizontal_speed(self.velocity));
        self.velocity[0]=speed*dx/length;self.velocity[2]=speed*dz/length;
        if rail.powered==Some(false) {
            if speed<0.03 {self.velocity=[0.;3];}
            else {self.velocity[0]*=0.5;self.velocity[1]=0.;self.velocity[2]*=0.5;}
        }
        let start=[x as f64+0.5+a[0] as f64*0.5,z as f64+0.5+a[2] as f64*0.5];
        let end=[x as f64+0.5+b[0] as f64*0.5,z as f64+0.5+b[2] as f64*0.5];
        let dx=end[0]-start[0];let dz=end[1]-start[1];
        let t=if dx==0. {self.pos[2]-z as f64} else if dz==0. {self.pos[0]-x as f64} else {((self.pos[0]-start[0])*dx+(self.pos[2]-start[1])*dz)*2.};
        self.pos=[start[0]+dx*t,py,start[1]+dz*t];
        self.move_by(w,[self.velocity[0].clamp(-max,max),0.,self.velocity[2].clamp(-max,max)]);
        let now=block(self.pos);
        for e in [a,b] {if e[1]!=0 && now.0-x==e[0] && now.2-z==e[2] {self.pos[1]+=e[1] as f64;break;}}
        let mut drag=0.98f32;
        if !loot_pending {drag+=(15-signal) as f32*0.001f32;}
        if self.in_water {drag*=0.95f32;}
        self.velocity[0]*=drag as f64;self.velocity[1]*=0.;self.velocity[2]*=drag as f64;
        if let (Some(old),Some(new))=(before,project(w,self.pos)) {
            let adjustment=(old[1]-new[1])*0.05;
            let speed=horizontal_speed(self.velocity);
            if speed>0. {let factor=(speed+adjustment)/speed;self.velocity[0]*=factor;self.velocity[2]*=factor;}
            self.pos[1]=new[1];
        }
        let now=block(self.pos);
        if now.0!=x || now.2!=z {
            let speed=horizontal_speed(self.velocity);
            self.velocity[0]=speed*(now.0-x) as f64;self.velocity[2]=speed*(now.2-z) as f64;
        }
        if rail.powered==Some(true) {
            let speed=horizontal_speed(self.velocity);
            if speed>0.01 {self.velocity[0]+=self.velocity[0]/speed*0.06;self.velocity[2]+=self.velocity[2]/speed*0.06;}
            else {match rail.shape {
                EastWest=>{if w.conductor((x-1,y,z)){self.velocity[0]=0.02;}else if w.conductor((x+1,y,z)){self.velocity[0] = -0.02;}},
                NorthSouth=>{if w.conductor((x,y,z-1)){self.velocity[2]=0.02;}else if w.conductor((x,y,z+1)){self.velocity[2] = -0.02;}}, _=>{}
            }}
        }
        // Detector, activator and plain rails all report `powered: None`; the
        // world tells them apart and ejects riders off a powered activator rail.
        let _ejected = w.activator_eject((x, y, z));
    }
}

#[cfg(test)]
#[path = "minecart_tests.rs"]
mod tests;

#[cfg(test)]
mod live_rail_tick_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    struct Scene {
        rails: HashMap<Pos, Rail>,
        conductors: HashSet<Pos>,
        eject: bool,
        eject_calls: Vec<Pos>,
    }

    impl Scene {
        fn new() -> Self {
            Self {
                rails: HashMap::new(),
                conductors: HashSet::new(),
                eject: false,
                eject_calls: Vec::new(),
            }
        }
    }

    impl World for Scene {
        fn rail(&mut self, at: Pos) -> Option<Rail> {
            self.rails.get(&at).copied()
        }
        fn conductor(&mut self, at: Pos) -> bool {
            self.conductors.contains(&at)
        }
        fn move_cart(&mut self, _pos: [f64; 3], delta: [f64; 3]) -> ([f64; 3], bool) {
            (delta, true)
        }
        fn activator_eject(&mut self, at: Pos) -> usize {
            self.eject_calls.push(at);
            usize::from(self.eject)
        }
    }

    fn straight_rails(scene: &mut Scene, powered: Option<bool>, z: std::ops::RangeInclusive<i32>) {
        for bz in z {
            scene.rails.insert(
                (0, 200, bz),
                Rail {
                    shape: Shape::NorthSouth,
                    powered,
                },
            );
        }
    }

    fn cart_on_rail() -> State {
        State {
            pos: [0.5, 200.5, 0.5],
            velocity: [0.0, 0.0, 0.2],
            on_ground: true,
            in_water: false,
        }
    }

    #[test]
    fn v1211_veh_ai_powered_rail_boost_runs_through_live_tick() {
        let mut powered = Scene::new();
        straight_rails(&mut powered, Some(true), -8..=64);
        let mut plain = Scene::new();
        straight_rails(&mut plain, None, -8..=64);
        let (mut powered_cart, mut plain_cart) = (cart_on_rail(), cart_on_rail());
        // The powered rail's 6% per-tick shove must outpace plain-rail drag.
        for _ in 0..20 {
            powered_cart.tick(&mut powered, false, 0);
            plain_cart.tick(&mut plain, false, 0);
        }
        assert!(powered_cart.velocity[2] > plain_cart.velocity[2]);
        assert!(powered_cart.velocity[2] > 0.4);
        // A stationary cart parked on a powered rail is kicked by its
        // conductor through the same live tick.
        let mut parked = Scene::new();
        straight_rails(&mut parked, Some(true), -8..=64);
        parked.conductors.insert((0, 200, -1));
        let mut cart = State {
            velocity: [0.0; 3],
            ..cart_on_rail()
        };
        cart.tick(&mut parked, false, 0);
        assert_eq!(cart.velocity[2], 0.02);
    }

    #[test]
    fn v1211_veh_ai_curved_rail_turn_runs_through_live_tick() {
        let mut scene = Scene::new();
        // SouthEast links the +z and +x exits; a cart rolling +x must be
        // redirected onto the diagonal through the live projection.
        scene.rails.insert(
            (0, 200, 0),
            Rail {
                shape: Shape::SouthEast,
                powered: None,
            },
        );
        let mut cart = State {
            pos: [0.5, 200.5, 0.5],
            velocity: [0.2, 0.0, 0.0],
            on_ground: true,
            in_water: false,
        };
        cart.tick(&mut scene, false, 0);
        assert!(cart.velocity[0] > 0.0, "curve dropped forward motion");
        assert!(cart.velocity[2] < 0.0, "curve did not turn the cart");
    }

    #[test]
    fn v1211_veh_ai_activator_rail_eject_runs_through_live_tick() {
        let mut scene = Scene::new();
        straight_rails(&mut scene, None, -8..=64);
        scene.eject = true;
        let mut cart = cart_on_rail();
        cart.tick(&mut scene, false, 0);
        // The live tick reports the rail block to the world, which owns the
        // passenger list and performs the eject.
        assert!(scene.eject_calls.contains(&(0, 200, 0)));
        assert_eq!(scene.eject_calls.len(), 1);
        // Off the rail the tick takes the derail path and never consults the
        // activator hook.
        scene.eject_calls.clear();
        let mut off = State {
            pos: [40.5, 200.5, 40.5],
            ..cart_on_rail()
        };
        off.tick(&mut scene, false, 0);
        assert!(scene.eject_calls.is_empty());
    }
}
