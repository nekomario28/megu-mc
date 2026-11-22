package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Actual TreeFeature.updateLeaves followed by StructureTemplate.updateShapeAtEdge. */
public final class LeafEdgeOracle {
    public static void main(String[] args) throws Exception {
        LeafDistanceOracle.cls("ab").getMethod("a").invoke(null);
        LeafDistanceOracle.cls("akt").getMethod("a").invoke(null);
        LeafDistanceOracle.registry=LeafDistanceOracle.cls("lt").getField("e").get(null);
        VanillaBlockTags.load(LeafDistanceOracle.registry);
        Class<?> pos=LeafDistanceOracle.cls("jd"), state=LeafDistanceOracle.cls("dtc");
        Class<?> level=LeafDistanceOracle.cls("dcx"), prop=LeafDistanceOracle.cls("duf");
        LeafDistanceOracle.x=pos.getMethod("u"); LeafDistanceOracle.y=pos.getMethod("v"); LeafDistanceOracle.z=pos.getMethod("w");
        LeafDistanceOracle.get=state.getMethod("c",prop); LeafDistanceOracle.set=state.getMethod("a",prop,Comparable.class);
        LeafDistanceOracle.distance=LeafDistanceOracle.cls("dki").getField("d").get(null);
        LeafDistanceOracle.persistent=LeafDistanceOracle.cls("dki").getField("e").get(null);
        LeafDistanceOracle.waterlogged=LeafDistanceOracle.cls("dki").getField("f").get(null);
        LeafDistanceOracle.air=LeafDistanceOracle.state("A");
        Constructor<?> pctor=pos.getConstructor(int.class,int.class,int.class);
        Constructor<?> bctor=LeafDistanceOracle.cls("ejj").getConstructor(int.class,int.class,int.class,int.class,int.class,int.class);
        Method update=LeafDistanceOracle.cls("edq").getDeclaredMethod("a",level,LeafDistanceOracle.cls("ejj"),Set.class,Set.class,Set.class);
        update.setAccessible(true);
        Method edge=LeafDistanceOracle.cls("ent").getMethod("a",level,int.class,LeafDistanceOracle.cls("exl"),int.class,int.class,int.class);
        List<String> rows=new ArrayList<>();
        for(int kind=0;kind<39;kind++) {
            var scene=LeafDistanceOracle.scene(kind);
            var origin=new LeafDistanceOracle.P(-1024,70,512);
            Map<LeafDistanceOracle.P,Object> world=new HashMap<>();
            Map<LeafDistanceOracle.P,String> input=new HashMap<>();
            for(var e:scene.cells().entrySet()) {
                // This oracle isolates dry-leaf scheduling; fluid calls need their own world contract.
                String value=e.getValue();
                if(value.startsWith("W")) value="L"+value.substring(1);
                if(value.startsWith("B")) value="P"+value.substring(1);
                if(value.startsWith("C")) value="S";
                input.put(e.getKey(),value);
                world.put(e.getKey().plus(origin),LeafDistanceOracle.state(value));
            }
            List<String> scheduled=new ArrayList<>(); Set<String> seen=new HashSet<>();
            Object proxy=Proxy.newProxyInstance(LeafEdgeOracle.class.getClassLoader(),new Class<?>[]{level},(p,m,a)->{
                if(m.getName().equals("a_") && a.length==1) return world.getOrDefault(LeafDistanceOracle.point(a[0]),LeafDistanceOracle.air);
                if(m.getName().equals("a") && a.length==3 && pos.isInstance(a[0]) && state.isInstance(a[1])) {
                    world.put(LeafDistanceOracle.point(a[0]),a[1]); return true;
                }
                if(m.getName().equals("a") && a.length==3 && pos.isInstance(a[0]) && a[2] instanceof Integer) {
                    var q=LeafDistanceOracle.point(a[0]);
                    String item=(q.x()-origin.x())+","+(q.y()-origin.y())+","+(q.z()-origin.z())+":"+a[1]+":"+a[2];
                    if(seen.add(item)) scheduled.add(item);
                    return null;
                }
                throw new UnsupportedOperationException(m.toString());
            });
            Set<Object> logs=new HashSet<>(),blocked=new HashSet<>();
            for(var p:scene.logs()) {p=p.plus(origin);logs.add(pctor.newInstance(p.x(),p.y(),p.z()));}
            for(var p:scene.blocked()) {p=p.plus(origin);blocked.add(pctor.newInstance(p.x(),p.y(),p.z()));}
            var min=scene.min().plus(origin); var max=scene.max().plus(origin);
            Object shape=update.invoke(null,proxy,bctor.newInstance(min.x(),min.y(),min.z(),max.x(),max.y(),max.z()),logs,blocked,Set.of());
            Method full=LeafDistanceOracle.cls("exl").getMethod("b",int.class,int.class,int.class);
            List<LeafDistanceOracle.P> occupied=new ArrayList<>();
            for(int x=min.x();x<=max.x();x++) for(int y=min.y();y<=max.y();y++) for(int z=min.z();z<=max.z();z++)
                if((boolean)full.invoke(shape,x-min.x(),y-min.y(),z-min.z()))
                    occupied.add(new LeafDistanceOracle.P(x-origin.x(),y-origin.y(),z-origin.z()));
            String after=LeafDistanceOracle.cells(LeafDistanceOracle.leaves(world,origin));
            edge.invoke(null,proxy,3,shape,min.x(),min.y(),min.z());
            rows.add(kind+"\t"+scene.min()+"\t"+scene.max()+"\t"+LeafDistanceOracle.points(scene.logs())+"\t"
                    +LeafDistanceOracle.points(scene.blocked())+"\t"+LeafDistanceOracle.cells(input)+"\t"
                    +LeafDistanceOracle.points(occupied)+"\t"+after+"\t"+String.join(";",scheduled));
        }
        Files.write(Path.of(args[0]),rows);
        System.out.println("native leaf edge cases: "+rows.size());
    }
}
