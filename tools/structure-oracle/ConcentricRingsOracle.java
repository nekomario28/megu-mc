package dev.mcspine.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;

/** Calls actual ring generation, with native fixed-biome relocation on/off. */
public final class ConcentricRingsOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    static Object field(String owner,String name) throws Exception { Field f=cls(owner).getDeclaredField(name);f.setAccessible(true);return f.get(null); }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);cls("akt").getMethod("a").invoke(null);
        Object access=cls("ka").getMethod("a",cls("jz")).invoke(null,field("lt","aA"));
        Object pack=cls("ats").getMethod("b").invoke(null);
        Object manager=cls("atx").getConstructor(cls("ass"),List.class).newInstance(field("ass","b"),List.of(pack));
        access=cls("akm").getMethod("a",cls("aue"),cls("ka"),List.class).invoke(null,manager,access,field("akm","a"));
        Object settingsKey=field("dzd","c");
        Object settingsRegistry=access.getClass().getMethod("d",cls("akq")).invoke(access,settingsKey.getClass().getMethod("c").invoke(settingsKey));
        Object settings=settingsRegistry.getClass().getMethod("f",cls("akq")).invoke(settingsRegistry,settingsKey);
        Object noises=access.getClass().getMethod("d",cls("akq")).invoke(access,field("lu","aP"));
        Object noiseGetter=cls("jz").getMethod("q").invoke(noises);
        Object randomState=cls("dzm").getMethod("a",cls("dzd"),cls("jn"),long.class).invoke(null,settings,noiseGetter,0L);
        Object biomes=access.getClass().getMethod("d",cls("akq")).invoke(access,field("lu","aF"));
        Object holder=((Optional<?>)cls("jz").getMethod("c",int.class).invoke(biomes,0)).orElseThrow();
        Object source=cls("deh").getConstructor(cls("jm")).newInstance(holder);
        Constructor<?> ctor=cls("dva").getDeclaredConstructor(cls("dzm"),cls("dea"),long.class,long.class,List.class);ctor.setAccessible(true);
        Method generate=cls("dva").getDeclaredMethod("a",cls("jm"),cls("ekl"));generate.setAccessible(true);
        Constructor<?> placement=cls("ekl").getConstructor(int.class,int.class,int.class,cls("jq"));
        Method direct=cls("jq").getMethod("a",List.class);
        Field x=cls("dcd").getField("e"),z=cls("dcd").getField("f");
        long[] seeds={0L,1L,-1L,42L,Long.MIN_VALUE,Long.MAX_VALUE};
        int[][] configs={{32,3,128},{1,1,1},{7,2,19},{0,3,9},{1023,7,43},{32,3,0},{7,0,3}};
        List<String> rows=new ArrayList<>();
        for(long seed:seeds) for(int[] config:configs) for(int relocate=0;relocate<2;relocate++) {
            Object state=ctor.newInstance(randomState,source,seed,seed,List.of());
            Object preferred=direct.invoke(null,relocate==0?List.of():List.of(holder));
            Object ring=placement.newInstance(config[0],config[1],config[2],preferred);
            var future=(CompletableFuture<?>)generate.invoke(state,null,ring);
            var points=(List<?>)future.get(60,TimeUnit.SECONDS);
            List<String> coordinates=new ArrayList<>();
            for(Object point:points)coordinates.add(x.getInt(point)+","+z.getInt(point));
            rows.add(seed+"\t"+config[0]+"\t"+config[1]+"\t"+config[2]+"\t"+relocate+"\t"+(coordinates.isEmpty()?"EMPTY":String.join(";",coordinates)));
        }
        Files.write(Path.of(args[0]),rows);System.out.println("native ring/fixed-biome relocation cases: "+rows.size());
    }
}
