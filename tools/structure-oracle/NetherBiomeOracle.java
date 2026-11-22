package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;

/** Real MultiNoiseBiomeSource and Nether RandomState from the jar's registry. */
public final class NetherBiomeOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    static Object field(String owner,String name) throws Exception { Field f=cls(owner).getDeclaredField(name);f.setAccessible(true);return f.get(null); }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);cls("akt").getMethod("a").invoke(null);
        Object access=cls("ka").getMethod("a",cls("jz")).invoke(null,field("lt","aA"));
        Object pack=cls("ats").getMethod("b").invoke(null);
        Object manager=cls("atx").getConstructor(cls("ass"),List.class).newInstance(field("ass","b"),List.of(pack));
        access=cls("akm").getMethod("a",cls("aue"),cls("ka"),List.class).invoke(null,manager,access,field("akm","a"));
        Object key=field("dzd","f");
        Object registry=access.getClass().getMethod("d",cls("akq")).invoke(access,key.getClass().getMethod("c").invoke(key));
        Object settings=registry.getClass().getMethod("f",cls("akq")).invoke(registry,key);
        Object noises=access.getClass().getMethod("d",cls("akq")).invoke(access,field("lu","aP"));
        Object noiseGetter=cls("jz").getMethod("q").invoke(noises);
        Object biomes=access.getClass().getMethod("d",cls("akq")).invoke(access,field("lu","aF"));
        Object list=cls("dek").getConstructor(cls("dek$a"),cls("jn")).newInstance(field("dek$a","a"),cls("jz").getMethod("q").invoke(biomes));
        Object parameters=cls("dek").getMethod("a").invoke(list);
        Object source=cls("dej").getMethod("a",cls("def$c")).invoke(null,parameters);
        Method create=cls("dzm").getMethod("a",cls("dzd"),cls("jn"),long.class);
        Method sample=cls("dej").getMethod("getNoiseBiome",int.class,int.class,int.class,cls("def$f"));
        long[] seeds={0L,1L,-1L,42L,Long.MIN_VALUE,Long.MAX_VALUE,0x123456789abcdef0L};
        List<int[]> points=new ArrayList<>(List.of(new int[]{0,0},new int[]{-1,1},new int[]{7500000,-7500000},new int[]{Integer.MIN_VALUE,Integer.MAX_VALUE}));
        Random random=new Random(1211003);
        for(int i=0;i<256;i++)points.add(new int[]{random.nextInt(16385)-8192,random.nextInt(16385)-8192});
        List<String> rows=new ArrayList<>(),surfaceRows=new ArrayList<>();Set<String> seen=new TreeSet<>();
        Method surfaceDepth=cls("dzq").getDeclaredMethod("a",int.class,int.class);surfaceDepth.setAccessible(true);
        Method randomFactory=cls("dzm").getMethod("a",cls("akr"));
        Method at=cls("dzl").getMethod("a",int.class,int.class,int.class);
        Method nextFloat=cls("ayw").getMethod("i");
        for(long seed:seeds) {
            Object state=create.invoke(null,settings,noiseGetter,seed);
            Object sampler=cls("dzm").getMethod("b").invoke(state);
            Object surface=cls("dzm").getMethod("c").invoke(state);
            for(int[] p:points) {
                int depth=(int)surfaceDepth.invoke(surface,p[0],p[1]);
                for(String name:new String[]{"minecraft:bedrock_floor","minecraft:bedrock_roof"}) {
                    Object id=cls("akr").getMethod("a",String.class).invoke(null,name);
                    Object factory=randomFactory.invoke(state,id);
                    for(int y:new int[]{1,2,3,4,123,124,125,126}) {
                        float value=(float)nextFloat.invoke(at.invoke(factory,p[0],y,p[1]));
                        surfaceRows.add(seed+"\t"+p[0]+"\t"+p[1]+"\t"+depth+"\t"+name+"\t"+y+"\t"+Integer.toUnsignedString(Float.floatToRawIntBits(value),16));
                    }
                }
            }
            for(int[] p:points) for(int y:new int[]{-16,0,32}) {
                Object holder=sample.invoke(source,p[0],y,p[1],sampler);
                Object biomeKey=((Optional<?>)holder.getClass().getMethod("e").invoke(holder)).orElseThrow();
                String name=biomeKey.getClass().getMethod("a").invoke(biomeKey).toString();seen.add(name);
                rows.add(seed+"\t"+p[0]+"\t"+y+"\t"+p[1]+"\t"+name);
            }
        }
        if(!seen.equals(Set.of("minecraft:nether_wastes","minecraft:soul_sand_valley","minecraft:crimson_forest","minecraft:warped_forest","minecraft:basalt_deltas")))throw new IllegalStateException("biome coverage: "+seen);
        Files.write(Path.of(args[0]),rows);System.out.println("native Nether biome cases: "+rows.size()+"; "+seen);
        Files.write(Path.of(args[1]),surfaceRows);System.out.println("native Nether surface/random cases: "+surfaceRows.size());
    }
}
