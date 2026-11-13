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
        if self.velocity[0]*dx+self.velocity[2]*dz<0. {dx=-dx;dz=-dz;}
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
                EastWest=>{if w.conductor((x-1,y,z)){self.velocity[0]=0.02;}else if w.conductor((x+1,y,z)){self.velocity[0]=-0.02;}},
                NorthSouth=>{if w.conductor((x,y,z-1)){self.velocity[2]=0.02;}else if w.conductor((x,y,z+1)){self.velocity[2]=-0.02;}}, _=>{}
            }}
        }
    }
}

#[cfg(test)]
#[path = "minecart_tests.rs"]
mod tests;
