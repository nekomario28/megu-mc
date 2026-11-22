package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Exact state predicates used by mineshaft floors/log/chain anchors. Keeping
 * these separate avoids substituting render, full collision or lichen support.
 * Tag membership (UNSTABLE_BOTTOM_CENTER) remains datapack-owned at the caller.
 */
public final class MineshaftSupportOracle {
    public static void main(String[] args) throws Exception {
        NativeHarness.bootstrap();
        Class<?> state=NativeHarness.C("dtc"), getter=NativeHarness.C("dcc");
        Class<?> pos=NativeHarness.C("jd"), direction=NativeHarness.C("ji");
        Class<?> support=NativeHarness.C("doa");
        Object zero=NativeHarness.pos(0,0,0);
        Object down=direction.getField("a").get(null);
        Object[] faces=direction.getEnumConstants();
        Object centre=support.getField("b").get(null);
        Method fullUp=state.getMethod("d",getter,pos,direction);
        Method centreDown=state.getMethod("a",getter,pos,direction,support);
        Method block=state.getMethod("b");
        NativeHarness.Level level=NativeHarness.level((x,y,z)->"minecraft:stone");
        Object world=Proxy.newProxyInstance(MineshaftSupportOracle.class.getClassLoader(),new Class<?>[]{getter},(p,m,a)->{
            // Like BlockPredicateOracle: the authored world contains no block
            // entities, including when a moving-piston state's shape asks.
            if(m.getName().equals("c_"))return null;
            try { return m.invoke(level.proxy,a); }
            catch(InvocationTargetException e) { throw e.getCause(); }
        });
        List<String> rows=new ArrayList<>();
        for(Object s:(Iterable<?>)NativeHarness.C("dfy").getField("q").get(null)) {
            int flags=0;
            for(int f=0;f<faces.length;f++)
                if((boolean)fullUp.invoke(s,world,zero,faces[f]))flags|=1<<f;
            if((boolean)centreDown.invoke(s,world,zero,down,centre))flags|=64;
            if(NativeHarness.C("diu").isInstance(block.invoke(s)))flags|=128;
            rows.add(s.toString()+"\t"+flags+"\t0\t0");
        }
        Files.write(Path.of(args[0]),rows);
        System.out.println(rows.size()+" native mineshaft support rows");
    }
}
