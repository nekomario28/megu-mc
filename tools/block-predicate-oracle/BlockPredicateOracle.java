package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.function.Predicate;

/** Pinned state predicates: bit 0 solid render, then WORLD_SURFACE,
 * OCEAN_FLOOR, MOTION_BLOCKING, MOTION_BLOCKING_NO_LEAVES, DoublePlantBlock,
 * a water fluid state, and a source fluid state. WG variants
 * use the same state predicate; their lifetime is a separate chunk contract.
 * No game signatures are removed and unknown proxy calls fail closed.
 */
public class BlockPredicateOracle {
    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Class<?> state=Class.forName("dtc"), getter=Class.forName("dcc"), pos=Class.forName("jd");
        Method render=state.getMethod("i",getter,pos);
        Object zero=pos.getConstructor(int.class,int.class,int.class).newInstance(0,0,0);
        Object stone=Class.forName("dfy").getMethod("o").invoke(Class.forName("dga").getField("b").get(null));
        Object world=Proxy.newProxyInstance(BlockPredicateOracle.class.getClassLoader(),new Class<?>[]{getter},(p,m,a)->{
            if(m.getName().equals("a_")) return stone;
            throw new UnsupportedOperationException(m.toString());
        });
        Class<?> heightmap=Class.forName("dyy$a");
        List<Predicate<Object>> predicates=new ArrayList<>();
        for(String field:new String[]{"b","d","e","f"}) {
            predicates.add((Predicate<Object>)heightmap.getMethod("e").invoke(heightmap.getField(field).get(null)));
        }
        Class<?> doublePlant=Class.forName("dig"), waterFluid=Class.forName("epk");
        Method block=state.getMethod("b"), fluid=state.getMethod("u"), fluidType=Class.forName("epe").getMethod("a");
        Method source=Class.forName("epe").getMethod("b");
        Class<?> direction=Class.forName("ji");
        Method attach=Class.forName("dky").getMethod("a",getter,direction,pos,state);
        Object empty=Class.forName("dcl").getField("a").get(null);
        List<String> rows=new ArrayList<>();
        for(Object s:(Iterable<?>)Class.forName("dfy").getField("q").get(null)) {
            int flags=(boolean)render.invoke(s,world,zero) ? 1 : 0;
            for(int i=0;i<predicates.size();i++) if(predicates.get(i).test(s)) flags|=2<<i;
            if(doublePlant.isInstance(block.invoke(s))) flags|=32;
            Object fs=fluid.invoke(s);
            if(waterFluid.isInstance(fluidType.invoke(fs))) flags|=64;
            if((boolean)source.invoke(fs)) flags|=128;
            int faces=0;
            Object[] directions=direction.getEnumConstants();
            for(int d=0;d<directions.length;d++)
                if((boolean)attach.invoke(null,empty,directions[d],zero,s)) faces|=1<<d;
            rows.add(s.toString()+"\t"+flags+"\t"+faces);
        }
        Files.write(Path.of(args[0]),rows);
        System.out.println(rows.size()+" native state predicate rows");
    }
}
