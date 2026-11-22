package dev.megu.tools;

import java.lang.reflect.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.MessageDigest;
import java.util.*;

/** Actual pinned MonsterRoomFeature over a stone volume with one two-cell hole.
 * Real chest/spawner entities preserve loot-seed and mob-selection RNG effects.
 * Reflection avoids sharing the signed game's default package; signatures stay intact.
 */
public class MonsterRoomOracle {
    static Method x, y, z, isAir, isBlock, getBlock, getProperty;
    static Object chestBlock, facing;
    static Constructor<?> rngCtor, worldgenCtor;
    static Object random(long seed, boolean wrapped) throws Exception {
        Object source=rngCtor.newInstance(seed);
        return wrapped ? worldgenCtor.newInstance(source) : source;
    }
    static int coord(Method m, Object p) throws Exception { return (int)m.invoke(p); }
    static String key(Object p) throws Exception {
        return coord(x,p) + "," + coord(y,p) + "," + coord(z,p);
    }
    static String describe(Object s) throws Exception {
        String name = getBlock.invoke(s).toString().replace("Block{", "").replace("}", "");
        return name + ":" + ((boolean)isBlock.invoke(s,chestBlock)
                ? getProperty.invoke(s,facing).toString() : "-");
    }
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        VanillaBlockTags.load(Class.forName("lt").getField("e").get(null));
        Class<?> pos = Class.forName("jd"), state = Class.forName("dtc"), block = Class.forName("dfy");
        Class<?> blocks = Class.forName("dga"), random = Class.forName("ayw"), xor = Class.forName("dzz");
        Class<?> level = Class.forName("dds"), context = Class.forName("ecg"), config = Class.forName("eep");
        x=pos.getMethod("u"); y=pos.getMethod("v"); z=pos.getMethod("w");
        isAir=state.getMethod("i"); isBlock=state.getMethod("a",block); getBlock=state.getMethod("b");
        getProperty=state.getMethod("c",Class.forName("duf")); // Property
        facing=Class.forName("dju").getField("aE").get(null);
        chestBlock=blocks.getField("cv").get(null);
        Object spawnerBlock=blocks.getField("ct").get(null);
        Object stone=block.getMethod("o").invoke(blocks.getField("b").get(null));
        Object air=block.getMethod("o").invoke(blocks.getField("nc").get(null));
        Constructor<?> chest=Class.forName("dqo").getConstructor(pos,state);
        Constructor<?> spawner=Class.forName("drw").getConstructor(pos,state);
        Method immutable=pos.getMethod("j"), nextInt=random.getMethod("a",int.class);
        rngCtor=xor.getConstructor(long.class);
        worldgenCtor=Class.forName("dzx").getConstructor(random);
        Constructor<?> ctxCtor=context.getConstructor(Optional.class,level,Class.forName("duz"),random,pos,Class.forName("eei"));
        Object feature=Class.forName("ecv").getConstructor(Class.forName("com.mojang.serialization.Codec"))
                .newInstance(config.getField("a").get(null));
        Method place=feature.getClass().getMethod("a",context);
        List<String> rows = new ArrayList<>();
        for (boolean wrapped : new boolean[]{false,true}) for (long seed : new long[]{0, 1, 2, 60007}) {
            int xr=(int)nextInt.invoke(random(seed,wrapped),2)+2;
            Map<String,Object> writes=new HashMap<>(), entities=new HashMap<>();
            Object world=Proxy.newProxyInstance(MonsterRoomOracle.class.getClassLoader(),new Class<?>[]{level},(proxy,m,a)->{
                String n=m.getName();
                if(n.equals("a_") && a.length==1) {
                    Object p=a[0];
                    return writes.getOrDefault(key(p),coord(x,p)==-xr-1 && coord(z,p)==0
                            && (coord(y,p)==16 || coord(y,p)==17) ? air : stone);
                }
                if(n.equals("u") && a.length==1) {
                    Object s=level.getMethod("a_",pos).invoke(proxy,a[0]);
                    return isAir.invoke(s);
                }
                if(n.equals("a") && a.length==3 && pos.isInstance(a[0]) && state.isInstance(a[1]) && a[2] instanceof Integer) {
                    Object p=immutable.invoke(a[0]), s=a[1]; writes.put(key(p),s);
                    if((boolean)isBlock.invoke(s,chestBlock)) entities.put(key(p),chest.newInstance(p,s));
                    if((boolean)isBlock.invoke(s,spawnerBlock)) entities.put(key(p),spawner.newInstance(p,s));
                    return true;
                }
                if(n.equals("c_") && a.length==1) return entities.get(key(a[0]));
                if(n.equals("I_") && (a==null || a.length==0)) return -64;
                throw new UnsupportedOperationException(m.toString());
            });
            Object rng=random(seed,wrapped);
            Object ctx=ctxCtor.newInstance(Optional.empty(),world,null,rng,
                    pos.getConstructor(int.class,int.class,int.class).newInstance(0,16,0),config.getField("b").get(null));
            boolean placed=(boolean)place.invoke(feature,ctx);
            List<String> cells=new ArrayList<>(); int chests=0;
            for(var e:writes.entrySet()) {
                cells.add(e.getKey()+":"+describe(e.getValue())+"\n");
                if((boolean)isBlock.invoke(e.getValue(),chestBlock)) chests++;
            }
            Collections.sort(cells);
            String digest=HexFormat.of().formatHex(MessageDigest.getInstance("MD5")
                    .digest(String.join("",cells).getBytes(StandardCharsets.UTF_8)));
            rows.add(seed+"\t"+placed+"\t"+writes.size()+"\t"+chests+"\t"+digest+"\t"+random.getMethod("g").invoke(rng)+"\t"+(wrapped ? "worldgen" : "raw"));
        }
        Files.write(Path.of(args[0]),rows);
        System.out.println("monster room oracle: "+rows.size()+" actual feature rows");
    }
}
