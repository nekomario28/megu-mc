use super::*;
struct Scene;
impl World for Scene {
    fn rail(&mut self,(x,y,z):Pos)->Option<Rail>{
        (x==0&&y==200&&(-8..=8).contains(&z)).then_some(Rail{shape:Shape::NorthSouth,powered:None})
    }
    fn conductor(&mut self,_:Pos)->bool{false}
    fn move_cart(&mut self,pos:[f64;3],mut delta:[f64;3])->([f64;3],bool){
        // The native authored scene has an uninterrupted stone floor with top=200.
        let ground=delta[1]<0.&&pos[1]+delta[1]<200.;
        if ground {delta[1]=200.-pos[1];}
        (delta,ground)
    }
}
#[test]
fn native_live_entity_ticks_match_empty_pending_loot_and_off_rail_cases(){
    let mut states=[State{pos:[0.5,200.5,0.5],velocity:[0.,0.,0.2],on_ground:false,in_water:false};3];
    states[2].pos=[4.5,202.5,0.5];states[2].velocity=[0.2,0.,0.];
    let mut rows=[0;3];
    for line in include_str!("../tests/data/mineshaft_motion_1211.tsv").lines().skip(1){
        let fields:Vec<_>=line.split_whitespace().collect();
        assert_eq!(fields.len(),8);
        let case:usize=fields[0].parse().unwrap();let tick:usize=fields[1].parse().unwrap();
        assert_eq!(tick,rows[case]);rows[case]+=1;
        if tick>0{states[case].tick(&mut Scene,case==1,0);}
        for (i,actual) in states[case].pos.into_iter().chain(states[case].velocity).enumerate(){
            let expected:f64=fields[i+2].parse().unwrap();
            assert!((actual-expected).abs()<1e-12,"case={case} tick={tick} component={i}: {actual} != {expected}");
        }
    }
    assert_eq!(rows,[13;3]);
}
