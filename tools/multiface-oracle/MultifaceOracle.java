package dev.mcspine.tools;

import com.google.gson.JsonParser;
import com.google.gson.JsonElement;
import com.mojang.serialization.DynamicOps;
import com.mojang.serialization.Codec;
import com.mojang.serialization.JsonOps;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/** Actual MultifaceGrowthFeature and default MultifaceSpreader, including real
 * ProtoChunks for postprocessing. All other world calls fail closed.
 */
public class MultifaceOracle {
    static Class<?> cls(String n) throws Exception { return Class.forName(n); }
    static Method x,y,z,property;
    static Object blockRegistry;
    static Object state(String name) throws Exception {
        Object id=cls("akr").getMethod("a",String.class).invoke(null,"minecraft:"+name);
        Object b=cls("jz").getMethod("a",cls("akr")).invoke(blockRegistry,id);
        return cls("dfy").getMethod("o").invoke(b);
    }
    static String key(Object p) throws Exception { return x.invoke(p)+","+y.invoke(p)+","+z.invoke(p); }
    static int mask(Object s) throws Exception {
        int bits=0; Object[] dirs=cls("ji").getEnumConstants();
        for(int d=0;d<dirs.length;d++) {
            Object prop=cls("dky").getMethod("b",cls("ji")).invoke(null,dirs[d]);
            if((boolean)property.invoke(s,prop)) bits|=1<<d;
        }
        if((boolean)property.invoke(s,cls("dts").getField("C").get(null))) bits|=64;
        return bits;
    }
    static String config(int kind,float chance) {
        String on=kind==5 ? "oak_leaves" : kind==6 ? "oak_slab" : "stone";
        return "{\"block\":\"minecraft:glow_lichen\",\"search_range\":20,\"can_place_on_ceiling\":true,"
            +"\"can_place_on_floor\":"+(kind==7)+",\"can_place_on_wall\":true,\"chance_of_spreading\":"+chance
            +",\"can_be_placed_on\":[\"minecraft:"+on+"\"]}";
    }
    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null); cls("akt").getMethod("a").invoke(null);
        blockRegistry=cls("lt").getField("e").get(null);
        Object core=cls("ka").getMethod("a",cls("jz")).invoke(null,cls("lt").getField("aA").get(null));
        Object pack=cls("ats").getMethod("b").invoke(null);
        Object manager=cls("atx").getConstructor(cls("ass"),List.class).newInstance(cls("ass").getField("b").get(null),List.of(pack));
        Object access=cls("akm").getMethod("a",cls("aue"),cls("ka"),List.class).invoke(null,manager,core,cls("akm").getField("a").get(null));
        Object biomes=access.getClass().getMethod("d",cls("akq")).invoke(access,cls("lu").getField("aF").get(null));
        DynamicOps<JsonElement> ops=(DynamicOps<JsonElement>)cls("akp").getMethod("a",DynamicOps.class,cls("jo$a")).invoke(null,JsonOps.INSTANCE,core);
        Object height=cls("dcy").getMethod("e",int.class,int.class).invoke(null,-64,384);
        Constructor<?> chunkCtor=cls("dvs").getConstructor(cls("dcd"),cls("dvv"),cls("dcy"),cls("jz"),cls("eab"));
        Object upgrade=cls("dvv").getField("a").get(null);
        Class<?> pos=cls("jd"),bs=cls("dtc"),level=cls("dds"),random=cls("ayw");
        x=pos.getMethod("u");y=pos.getMethod("v");z=pos.getMethod("w");property=bs.getMethod("c",cls("duf"));
        Object air=state("air"),stone=state("stone"),water=state("water"),leaves=state("oak_leaves"),slab=state("oak_slab");
        Object flowing=bs.getMethod("a",cls("duf"),Comparable.class).invoke(water,cls("dts").getField("aP").get(null),8);
        Object lichen=state("glow_lichen");
        Codec<?> codec=(Codec<?>)cls("een").getField("a").get(null);
        Object feature=cls("ecw").getConstructor(Codec.class).newInstance(codec);
        Method place=feature.getClass().getMethod("a",cls("ecg"));
        Constructor<?> context=cls("ecg").getConstructor(Optional.class,level,cls("duz"),random,pos,cls("eei"));
        Object origin=pos.getConstructor(int.class,int.class,int.class).newInstance(0,16,0);
        List<String> rows=new ArrayList<>();
        for(int kind=0;kind<10;kind++) for(long seed:new long[]{0,7,-1,60007}) for(float chance:new float[]{0,0.5f,1}) {
            final int scenario=kind;
            Object cfg=codec.parse(ops,JsonParser.parseString(config(kind,chance))).getOrThrow();
            Map<String,Object> writes=new HashMap<>(),chunks=new HashMap<>();
            Object world=Proxy.newProxyInstance(MultifaceOracle.class.getClassLoader(),new Class<?>[]{level},(proxy,m,a)->{
                String n=m.getName();
                if(n.equals("a_") && a.length==1) {
                    String k=key(a[0]); if(writes.containsKey(k)) return writes.get(k);
                    int px=(int)x.invoke(a[0]),py=(int)y.invoke(a[0])-16,pz=(int)z.invoke(a[0]);
                    if(Math.max(Math.abs(px),Math.max(Math.abs(py),Math.abs(pz)))>4) throw new AssertionError("read outside scenario: "+k);
                    if(scenario==9 && px==0 && py==0 && pz==0) return lichen;
                    if(scenario==0 || scenario==7) return px==0 && py==0 && pz==0 ? air : stone;
                    int plane=scenario==2 ? 2 : scenario==3 ? 3 : 1;
                    if(px==plane) return scenario==5 ? leaves : scenario==6 ? slab : stone;
                    return scenario==4 ? water : scenario==8 ? flowing : air;
                }
                if(n.equals("b_") && a.length==1 && pos.isInstance(a[0])) {
                    Object s=level.getMethod("a_",pos).invoke(proxy,a[0]);
                    return bs.getMethod("u").invoke(s);
                }
                if(n.equals("a") && a.length==3 && pos.isInstance(a[0]) && bs.isInstance(a[1])) {
                    writes.put(key(a[0]),a[1]);return true;
                }
                if(n.equals("y") && a.length==1 && pos.isInstance(a[0])) {
                    int cx=(int)x.invoke(a[0])>>4,cz=(int)z.invoke(a[0])>>4;
                    String k=cx+","+cz;
                    if(!chunks.containsKey(k)) chunks.put(k,chunkCtor.newInstance(cls("dcd").getConstructor(int.class,int.class).newInstance(cx,cz),upgrade,height,biomes,null));
                    return chunks.get(k);
                }
                throw new UnsupportedOperationException(m.toString());
            });
            Object rng=cls("dzx").getConstructor(random).newInstance(cls("dzz").getConstructor(long.class).newInstance(seed));
            boolean placed=(boolean)place.invoke(feature,context.newInstance(Optional.empty(),world,null,rng,origin,cfg));
            List<String> cells=new ArrayList<>();
            for(var e:writes.entrySet()) {
                String[] p=e.getKey().split(",");
                cells.add(p[0]+","+(Integer.parseInt(p[1])-16)+","+p[2]+":"+mask(e.getValue())+"\n");
            }
            Collections.sort(cells);
            String digest=HexFormat.of().formatHex(MessageDigest.getInstance("MD5").digest(String.join("",cells).getBytes(StandardCharsets.UTF_8)));
            rows.add(kind+"\t"+seed+"\t"+chance+"\t"+placed+"\t"+writes.size()+"\t"+random.getMethod("g").invoke(rng)+"\t"+digest);
        }
        ((AutoCloseable)manager).close();
        Files.write(Path.of(args[0]),rows);
        System.out.println("multiface oracle: "+rows.size()+" actual feature/spreader vectors");
    }
}
