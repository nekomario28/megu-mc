package dev.mcspine.tools;

import java.io.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.security.*;
import java.util.*;

/** Loads every native template, and hashes actual typed NBT and loaded block order. */
public final class StructureTemplateOracle {
    static Class<?> tag, compound, list;
    static Method tagId, write, keys, get, elementType, size, listGet;
    static void canonical(Object t, DataOutputStream out) throws Exception {
        int id=((Number)tagId.invoke(t)).intValue();
        if(id==10) {
            List<String> sorted=new ArrayList<>((Set<String>)keys.invoke(t));
            Collections.sort(sorted);
            for(String key:sorted) {
                Object value=get.invoke(t,key);
                out.writeByte(((Number)tagId.invoke(value)).intValue());
                out.writeUTF(key); canonical(value,out);
            }
            out.writeByte(0);
        } else if(id==9) {
            int n=((Number)size.invoke(t)).intValue();
            out.writeByte(n==0?0:((Number)elementType.invoke(t)).intValue()); out.writeInt(n);
            for(int i=0;i<n;i++) canonical(listGet.invoke(t,i),out);
        } else write.invoke(t,out);
    }
    static void named(Object t,DataOutputStream out) throws Exception {
        int id=((Number)tagId.invoke(t)).intValue(); out.writeByte(id);
        if(id!=0) out.writeUTF(""); canonical(t,out);
    }
    static String hash(byte[] bytes) throws Exception {
        return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(bytes));
    }
    static void initNbt() throws Exception {
        tag=Class.forName("uy"); compound=Class.forName("ub"); list=Class.forName("uh");
        tagId=tag.getMethod("b"); write=tag.getMethod("a",DataOutput.class);
        keys=compound.getMethod("e"); get=compound.getMethod("c",String.class);
        elementType=list.getMethod("f"); size=list.getMethod("size"); listGet=list.getMethod("k",int.class);
    }
    /** Shared canonical cell encoding for loader and processor oracles. */
    static void writeBlockInfo(Object info, DataOutputStream out) throws Exception {
        Class<?> infoClass=Class.forName("ent$c"), posClass=Class.forName("jd");
        Object p=infoClass.getMethod("a").invoke(info);
        for(String coordinate:new String[]{"u","v","w"}) out.writeInt((int)posClass.getMethod(coordinate).invoke(p));
        Object state=infoClass.getMethod("b").invoke(info);
        named(Class.forName("uq").getMethod("a",Class.forName("dtc")).invoke(null,state),out);
        Object nbt=infoClass.getMethod("c").invoke(info);
        out.writeBoolean(nbt!=null);
        if(nbt!=null) named(nbt,out);
    }
    public static void main(String[] args) throws Exception {
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        initNbt();
        Object registry=Class.forName("lt").getField("e").get(null);
        Object lookup=Class.forName("jz").getMethod("q").invoke(registry);
        Class<?> template=Class.forName("ent"),state=Class.forName("dtc"),pos=Class.forName("jd");
        Method read=Class.forName("uo").getMethod("a",Path.class,Class.forName("uk"));
        Method unlimited=Class.forName("uk").getMethod("a");
        Method load=template.getMethod("a",Class.forName("jn"),compound);
        Field palettes=template.getDeclaredField("l"), entities=template.getDeclaredField("m");
        palettes.setAccessible(true); entities.setAccessible(true);
        Method blocks=Class.forName("ent$a").getMethod("a");
        List<String> rows=new ArrayList<>();
        try(var paths=Files.walk(Path.of(args[0]))) {
            for(Path file:paths.filter(p->p.toString().endsWith(".nbt")).sorted().toList()) {
                Object root=read.invoke(null,file,unlimited.invoke(null));
                ByteArrayOutputStream raw=new ByteArrayOutputStream();
                named(root,new DataOutputStream(raw));
                Object loaded=template.getConstructor().newInstance(); load.invoke(loaded,lookup,root);
                List<?> ps=(List<?>)palettes.get(loaded);
                List<?> es=(List<?>)entities.get(loaded);
                ByteArrayOutputStream ordered=new ByteArrayOutputStream();
                DataOutputStream out=new DataOutputStream(ordered); out.writeInt(ps.size());
                for(Object p:ps) {
                    List<?> bs=(List<?>)blocks.invoke(p); out.writeInt(bs.size());
                    for(Object info:bs) writeBlockInfo(info,out);
                }
                rows.add(Path.of(args[0]).relativize(file).toString()+"\t"+hash(raw.toByteArray())+"\t"+ps.size()+"\t"+es.size()+"\t"+hash(ordered.toByteArray()));
            }
        }
        if(rows.size()!=1180) throw new IllegalStateException("incomplete native templates "+rows.size());
        Files.write(Path.of(args[1]),rows);
        Object empty=Class.forName("dcl").getField("a").get(null);
        Object zero=pos.getConstructor(int.class,int.class,int.class).newInstance(0,0,0);
        Method block=state.getMethod("b"), dynamic=Class.forName("dfy").getMethod("p");
        Method full=state.getMethod("r",Class.forName("dcc"),pos);
        rows.clear();
        for(Object s:(Iterable<?>)Class.forName("dfy").getField("q").get(null)) {
            boolean solid=!(boolean)dynamic.invoke(block.invoke(s)) && (boolean)full.invoke(s,empty,zero);
            rows.add(s.toString()+"\t"+(solid?1:0));
        }
        Files.write(Path.of(args[2]),rows);
        System.out.println("native templates: 1180; native template-solid states: "+rows.size());
    }
}
