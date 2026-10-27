package dev.mcspine.tools;

import com.mojang.serialization.Codec;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/** Actual GlowstoneFeature over controlled ceilings and blocked cavities. */
public final class GlowstoneOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    static Object block(String name) throws Exception {
        Object id=cls("akr").getMethod("a",String.class).invoke(null,"minecraft:"+name);
        Object block=cls("jz").getMethod("a",cls("akr")).invoke(cls("lt").getField("e").get(null),id);
        return cls("dfy").getMethod("o").invoke(block);
    }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);cls("akt").getMethod("a").invoke(null);
        Class<?> pos=cls("jd"),state=cls("dtc"),world=cls("dds"),random=cls("ayw");
        Object codec=cls("eep").getField("a").get(null),cfg=cls("eep").getField("b").get(null);
        Object feature=cls("ecl").getConstructor(Codec.class).newInstance(codec);
        Method place=cls("ecl").getMethod("a",cls("ecg"));
        Constructor<?> context=cls("ecg").getConstructor(Optional.class,world,cls("duz"),random,pos,cls("eei"));
        Method x=pos.getMethod("u"),y=pos.getMethod("v"),z=pos.getMethod("w");
        Object air=block("air"),netherrack=block("netherrack"),basalt=block("basalt"),blackstone=block("blackstone"),stone=block("stone"),glowstone=block("glowstone");
        List<String> rows=new ArrayList<>();
        for(int scenario=0;scenario<7;scenario++)for(long seed:new long[]{0,1,-1,42,60007,Long.MIN_VALUE,Long.MAX_VALUE}) {
            final int scene=scenario;
            Map<String,Object> writes=new HashMap<>();
            Object level=Proxy.newProxyInstance(GlowstoneOracle.class.getClassLoader(),new Class<?>[]{world},(proxy,m,a)->{
                String name=m.getName();
                if((name.equals("a_") || name.equals("u")) && a.length==1 && pos.isInstance(a[0])) {
                    int px=(int)x.invoke(a[0]),py=(int)y.invoke(a[0])-64,pz=(int)z.invoke(a[0]);
                    if(Math.abs(px)>8 || Math.abs(pz)>8 || py< -12 || py>1)throw new AssertionError("read outside controlled world");
                    String key=px+","+py+","+pz;
                    Object value=writes.get(key);
                    if(value==null) {
                        if(scene==4 && px==0 && py==0 && pz==0)value=stone;
                        else if(py==1 && scene!=5)value=scene==1?basalt:scene==2?blackstone:scene==3?stone:netherrack;
                        else if(scene==6 && py<0 && Math.floorMod(px+pz,3)==0)value=netherrack;
                        else value=air;
                    }
                    return name.equals("u")?value.equals(air):value;
                }
                if(name.equals("a") && a.length==3 && pos.isInstance(a[0]) && state.isInstance(a[1])) {
                    if(!a[2].equals(2) || !a[1].equals(glowstone))throw new AssertionError("unexpected glowstone write");
                    writes.put(x.invoke(a[0])+","+((int)y.invoke(a[0])-64)+","+z.invoke(a[0]),a[1]);return true;
                }
                throw new UnsupportedOperationException(m.toString());
            });
            Object rng=cls("dzx").getConstructor(random).newInstance(cls("dzz").getConstructor(long.class).newInstance(seed));
            boolean success=(boolean)place.invoke(feature,context.newInstance(Optional.empty(),level,null,rng,pos.getConstructor(int.class,int.class,int.class).newInstance(0,64,0),cfg));
            List<String> cells=new ArrayList<>();for(String key:writes.keySet())cells.add(key+"\n");Collections.sort(cells);
            String digest=HexFormat.of().formatHex(MessageDigest.getInstance("MD5").digest(String.join("",cells).getBytes(StandardCharsets.UTF_8)));
            rows.add(scene+"\t"+seed+"\t"+success+"\t"+writes.size()+"\t"+random.getMethod("g").invoke(rng)+"\t"+digest);
        }
        Files.write(Path.of(args[0]),rows);System.out.println("native glowstone feature cases: "+rows.size());
    }
}
