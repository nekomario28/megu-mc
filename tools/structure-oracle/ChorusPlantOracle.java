package dev.megu.tools;

import com.mojang.serialization.Codec;
import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/** Actual ChorusPlantFeature and recursive ChorusFlowerBlock generation. */
public final class ChorusPlantOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    static String stateKey(Object state) {
        String s=state.toString();
        if(s.startsWith("Block{minecraft:chorus_plant}")) {
            int mask=0,i=0;
            for(String direction:List.of("down","up","north","south","west","east")) {
                if(s.contains(direction+"=true"))mask|=1<<i;
                i++;
            }
            return "plant:"+mask;
        }
        if(s.equals("Block{minecraft:chorus_flower}[age=5]"))return "flower:5";
        throw new AssertionError("unexpected chorus state: "+state);
    }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);cls("akt").getMethod("a").invoke(null);
        Class<?> pos=cls("jd"),state=cls("dtc"),world=cls("dds"),random=cls("ayw");
        Object codec=cls("eep").getField("a").get(null),cfg=cls("eep").getField("b").get(null);
        Object feature=cls("ebp").getConstructor(Codec.class).newInstance(codec);
        Method place=cls("ebp").getMethod("a",cls("ecg"));
        Constructor<?> context=cls("ecg").getConstructor(Optional.class,world,cls("duz"),random,pos,cls("eei"));
        Method x=pos.getMethod("u"),y=pos.getMethod("v"),z=pos.getMethod("w");
        Method defaultState=cls("dfy").getMethod("o");
        Object endStone=defaultState.invoke(cls("dga").getField("fz").get(null));
        Object stone=defaultState.invoke(cls("dga").getField("b").get(null));
        Object air=defaultState.invoke(cls("dga").getField("a").get(null));
        List<String> rows=new ArrayList<>();
        for(int scenario=0;scenario<6;scenario++)for(long seed:new long[]{0,1,-1,42,60007,Long.MIN_VALUE,Long.MAX_VALUE}) {
            final int scene=scenario;
            Map<String,Object> writes=new HashMap<>();
            Object level=Proxy.newProxyInstance(ChorusPlantOracle.class.getClassLoader(),new Class<?>[]{world},(proxy,m,a)->{
                String name=m.getName();
                if((name.equals("a_") || name.equals("u")) && a.length==1 && pos.isInstance(a[0])) {
                    int px=(int)x.invoke(a[0]),py=(int)y.invoke(a[0])-64,pz=(int)z.invoke(a[0]);
                    if(Math.abs(px)>10 || Math.abs(pz)>10 || py< -4 || py>36)throw new AssertionError("read outside controlled world");
                    String key=px+","+py+","+pz;
                    Object value=writes.get(key);
                    if(value==null) {
                        boolean obstacle=scene==1 && Math.abs(px)+Math.abs(pz)==2 && py>=2 && py<=8
                            ||scene==3 && px==0 && py==0 && pz==0
                            ||scene==5 && py==4 && Math.abs(px)+Math.abs(pz)<=5;
                        if(obstacle)value=stone;
                        else if(py<0 && (scene!=4 || px==0 && pz==0))value=scene==2?stone:endStone;
                        else value=air;
                    }
                    return name.equals("u")?value.equals(air):value;
                }
                if(name.equals("a") && a.length==3 && pos.isInstance(a[0]) && state.isInstance(a[1])) {
                    if(!a[2].equals(2))throw new AssertionError("unexpected write flags");
                    stateKey(a[1]);
                    writes.put(x.invoke(a[0])+","+((int)y.invoke(a[0])-64)+","+z.invoke(a[0]),a[1]);return true;
                }
                throw new UnsupportedOperationException(m.toString());
            });
            Object rng=cls("dzx").getConstructor(random).newInstance(cls("dzz").getConstructor(long.class).newInstance(seed));
            boolean success=(boolean)place.invoke(feature,context.newInstance(Optional.empty(),level,null,rng,pos.getConstructor(int.class,int.class,int.class).newInstance(0,64,0),cfg));
            List<String> cells=new ArrayList<>();
            for(var e:writes.entrySet())cells.add(e.getKey()+":"+stateKey(e.getValue())+"\n");
            Collections.sort(cells);
            String digest=HexFormat.of().formatHex(MessageDigest.getInstance("MD5").digest(String.join("",cells).getBytes(StandardCharsets.UTF_8)));
            rows.add(scene+"\t"+seed+"\t"+success+"\t"+writes.size()+"\t"+random.getMethod("g").invoke(rng)+"\t"+digest);
        }
        Files.write(Path.of(args[0]),rows);System.out.println("native chorus feature cases: "+rows.size());
    }
}
