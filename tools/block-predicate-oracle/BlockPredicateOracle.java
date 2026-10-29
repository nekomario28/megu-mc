package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.function.Predicate;

/** Pinned block predicates: render/heightmap/plant/fluid bits, support-face bits,
 * and a separate collision-shape-full column. WG variants use the same state predicate;
 * their lifetime is a separate chunk contract. No game signatures are removed and unknown
 * proxy calls fail closed.
 */
public class BlockPredicateOracle {
    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        Class<?> state=Class.forName("dtc"), getter=Class.forName("dcc"), pos=Class.forName("jd");
        Method render=state.getMethod("i",getter,pos);
        Method fullCollision=state.getMethod("r",getter,pos);
        Object zero=pos.getConstructor(int.class,int.class,int.class).newInstance(0,0,0);
        Object stone=Class.forName("dfy").getMethod("o").invoke(Class.forName("dga").getField("b").get(null));
        Object empty=Class.forName("dcl").getField("a").get(null);
        Object world=Proxy.newProxyInstance(BlockPredicateOracle.class.getClassLoader(),new Class<?>[]{getter},(p,m,a)->{
            if(m.getName().equals("a_")) return stone;
            if(m.getName().equals("c_")) return null; // no block entities in the probe world
            if(m.getName().equals("b_")) return empty; // empty fluid at queried neighbours
            if(m.getName().equals("a") && m.getReturnType() == Optional.class) return Optional.empty();
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
        List<String> rows=new ArrayList<>();
        for(Object s:(Iterable<?>)Class.forName("dfy").getField("q").get(null)) {
            int flags=(boolean)render.invoke(s,world,zero) ? 1 : 0;
            boolean collisionFull=(boolean)fullCollision.invoke(s,world,zero);
            for(int i=0;i<predicates.size();i++) if(predicates.get(i).test(s)) flags|=2<<i;
            if(doublePlant.isInstance(block.invoke(s))) flags|=32;
            Object fs=fluid.invoke(s);
            if(waterFluid.isInstance(fluidType.invoke(fs))) flags|=64;
            if((boolean)source.invoke(fs)) flags|=128;
            int faces=0;
            Object[] directions=direction.getEnumConstants();
            for(int d=0;d<directions.length;d++)
                if((boolean)attach.invoke(null,empty,directions[d],zero,s)) faces|=1<<d;
            rows.add(s.toString()+"\t"+flags+"\t"+faces+"\t"+(collisionFull?1:0));
        }
        Files.write(Path.of(args[0]),rows);
        System.out.println(rows.size()+" native state predicate rows");
    }
}
