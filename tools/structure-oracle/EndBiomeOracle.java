package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Real TheEndBiomeSource, own-jar biomes, End settings and seeded RandomState. */
public final class EndBiomeOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    static Object field(String owner,String name) throws Exception { Field f=cls(owner).getDeclaredField(name);f.setAccessible(true);return f.get(null); }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);cls("akt").getMethod("a").invoke(null);
        Object access=cls("ka").getMethod("a",cls("jz")).invoke(null,field("lt","aA"));
        Object pack=cls("ats").getMethod("b").invoke(null);
        Object manager=cls("atx").getConstructor(cls("ass"),List.class).newInstance(field("ass","b"),List.of(pack));
        access=cls("akm").getMethod("a",cls("aue"),cls("ka"),List.class).invoke(null,manager,access,field("akm","a"));
        Object key=field("dzd","g");
        Object registry=access.getClass().getMethod("d",cls("akq")).invoke(access,key.getClass().getMethod("c").invoke(key));
        Object settings=registry.getClass().getMethod("f",cls("akq")).invoke(registry,key);
        Object noises=access.getClass().getMethod("d",cls("akq")).invoke(access,field("lu","aP"));
        Object noiseGetter=cls("jz").getMethod("q").invoke(noises);
        Object biomes=access.getClass().getMethod("d",cls("akq")).invoke(access,field("lu","aF"));
        Object source=cls("den").getMethod("a",cls("jn")).invoke(null,cls("jz").getMethod("q").invoke(biomes));
        Method create=cls("dzm").getMethod("a",cls("dzd"),cls("jn"),long.class);
        Method sample=cls("den").getMethod("getNoiseBiome",int.class,int.class,int.class,cls("def$f"));
        long[] seeds={0L,1L,-1L,42L,Long.MIN_VALUE,Long.MAX_VALUE,0x123456789abcdef0L};
        List<int[]> points=new ArrayList<>(List.of(new int[]{0,0},new int[]{255,0},new int[]{256,0},new int[]{257,0},new int[]{-255,0},new int[]{-256,0},new int[]{-257,0},new int[]{183,183},new int[]{184,184},new int[]{512,-512},new int[]{Integer.MIN_VALUE,Integer.MAX_VALUE},new int[]{7500000,-7500000}));
        Random random=new Random(1211002);
        for(int i=0;i<100;i++)points.add(new int[]{random.nextInt(16385)-8192,random.nextInt(16385)-8192});
        List<String> rows=new ArrayList<>();Set<String> seen=new TreeSet<>();
        for(long seed:seeds) {
            Object state=create.invoke(null,settings,noiseGetter,seed);
            Object sampler=cls("dzm").getMethod("b").invoke(state);
            for(int[] p:points) for(int y:new int[]{-16,0,32}) {
                Object holder=sample.invoke(source,p[0],y,p[1],sampler);
                Object biomeKey=((Optional<?>)holder.getClass().getMethod("e").invoke(holder)).orElseThrow();
                String name=biomeKey.getClass().getMethod("a").invoke(biomeKey).toString();seen.add(name);
                rows.add(seed+"\t"+p[0]+"\t"+y+"\t"+p[1]+"\t"+name);
            }
        }
        if(!seen.equals(Set.of("minecraft:the_end","minecraft:end_highlands","minecraft:end_midlands","minecraft:small_end_islands","minecraft:end_barrens")))throw new IllegalStateException("biome coverage: "+seen);
        Files.write(Path.of(args[0]),rows);System.out.println("native End biome cases: "+rows.size()+"; "+seen);
    }
}
