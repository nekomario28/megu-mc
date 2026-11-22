package dev.megu.tools;

import com.mojang.serialization.Codec;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/** Actual EndIslandFeature. Its world proxy accepts writes and rejects reads. */
public final class EndIslandOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);cls("akt").getMethod("a").invoke(null);
        Class<?> pos=cls("jd"),state=cls("dtc"),world=cls("dds"),random=cls("ayw");
        Object codec=cls("eep").getField("a").get(null),cfg=cls("eep").getField("b").get(null);
        Object feature=cls("ecb").getConstructor(Codec.class).newInstance(codec);
        Method place=cls("ecb").getMethod("a",cls("ecg"));
        Constructor<?> context=cls("ecg").getConstructor(Optional.class,world,cls("duz"),random,pos,cls("eei"));
        Method x=pos.getMethod("u"),y=pos.getMethod("v"),z=pos.getMethod("w");
        Object endStone=cls("dfy").getMethod("o").invoke(cls("dga").getField("fz").get(null));
        List<String> rows=new ArrayList<>();
        for(long seed:new long[]{0,1,-1,42,60007,Long.MIN_VALUE,Long.MAX_VALUE}) {
            Map<String,Object> writes=new HashMap<>();
            Object level=Proxy.newProxyInstance(EndIslandOracle.class.getClassLoader(),new Class<?>[]{world},(proxy,m,a)->{
                if(m.getName().equals("a") && a.length==3 && pos.isInstance(a[0]) && state.isInstance(a[1])) {
                    if(!a[2].equals(3))throw new AssertionError("unexpected write flags: "+a[2]);
                    if(!a[1].equals(endStone))throw new AssertionError("unexpected island state: "+a[1]);
                    String key=x.invoke(a[0])+","+((int)y.invoke(a[0])-64)+","+z.invoke(a[0])+"\n";
                    writes.put(key,a[1]);return true;
                }
                throw new UnsupportedOperationException(m.toString());
            });
            Object rng=cls("dzx").getConstructor(random).newInstance(cls("dzz").getConstructor(long.class).newInstance(seed));
            boolean success=(boolean)place.invoke(feature,context.newInstance(Optional.empty(),level,null,rng,pos.getConstructor(int.class,int.class,int.class).newInstance(0,64,0),cfg));
            List<String> cells=new ArrayList<>(writes.keySet());Collections.sort(cells);
            String digest=HexFormat.of().formatHex(MessageDigest.getInstance("MD5").digest(String.join("",cells).getBytes(StandardCharsets.UTF_8)));
            rows.add(seed+"\t"+success+"\t"+writes.size()+"\t"+random.getMethod("g").invoke(rng)+"\t"+digest);
        }
        Files.write(Path.of(args[0]),rows);System.out.println("native End island feature cases: "+rows.size());
    }
}
