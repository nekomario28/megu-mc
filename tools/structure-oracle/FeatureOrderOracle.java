package dev.megu.tools;

import com.google.gson.*;
import java.lang.reflect.*;
import java.nio.file.*;
import java.util.*;
import java.util.function.Function;

/** Native possible-biome iteration and FeatureSorter for each vanilla source. */
public final class FeatureOrderOracle {
    static Class<?> cls(String name) throws Exception { return Class.forName(name); }
    static Object field(String owner,String name) throws Exception { Field f=cls(owner).getDeclaredField(name);f.setAccessible(true);return f.get(null); }
    static String biomeName(Object holder) throws Exception {
        Object key=((Optional<?>)holder.getClass().getMethod("e").invoke(holder)).orElseThrow();
        return key.getClass().getMethod("a").invoke(key).toString();
    }
    public static void main(String[] args) throws Exception {
        cls("ab").getMethod("a").invoke(null);cls("akt").getMethod("a").invoke(null);
        Object access=cls("ka").getMethod("a",cls("jz")).invoke(null,field("lt","aA"));
        Object pack=cls("ats").getMethod("b").invoke(null);
        Object manager=cls("atx").getConstructor(cls("ass"),List.class).newInstance(field("ass","b"),List.of(pack));
        access=cls("akm").getMethod("a",cls("aue"),cls("ka"),List.class).invoke(null,manager,access,field("akm","a"));
        Object biomes=access.getClass().getMethod("d",cls("akq")).invoke(access,field("lu","aF"));
        Object getter=cls("jz").getMethod("q").invoke(biomes);
        Object placed=access.getClass().getMethod("d",cls("akq")).invoke(access,field("lu","aQ"));
        Method features=cls("ddx").getMethod("b"),settings=cls("ddw").getMethod("d");
        Function<Object,Object> generation=holder->{
            try { return features.invoke(settings.invoke(cls("jm").getMethod("a").invoke(holder))); }
            catch(Exception e) { throw new RuntimeException(e); }
        };
        JsonObject output=new JsonObject();
        for(String dimension:List.of("overworld","nether","end")) {
            Object source;
            if(dimension.equals("end"))source=cls("den").getMethod("a",cls("jn")).invoke(null,getter);
            else {
                Object preset=field("dek$a",dimension.equals("overworld")?"b":"a");
                Object list=cls("dek").getConstructor(cls("dek$a"),cls("jn")).newInstance(preset,getter);
                source=cls("dej").getMethod("a",cls("def$c")).invoke(null,cls("dek").getMethod("a").invoke(list));
            }
            List<?> possible=new ArrayList<>((Set<?>)cls("dea").getMethod("c").invoke(source));
            JsonObject item=new JsonObject();JsonArray names=new JsonArray(),steps=new JsonArray();
            for(Object holder:possible)names.add(biomeName(holder));
            List<?> sorted=(List<?>)cls("deg").getMethod("a",List.class,Function.class,boolean.class).invoke(null,possible,generation,true);
            for(Object step:sorted) {
                JsonArray entries=new JsonArray();
                for(Object feature:(List<?>)cls("deg$b").getMethod("a").invoke(step)) {
                    Object id=cls("jz").getMethod("b",Object.class).invoke(placed,feature);
                    if(id==null)throw new IllegalStateException("unregistered placed feature");
                    entries.add(id.toString());
                }
                steps.add(entries);
            }
            item.add("biomes",names);item.add("steps",steps);output.add(dimension,item);
            System.out.println(dimension+": "+names.size()+" possible biomes; "+steps.size()+" steps");
        }
        ((AutoCloseable)manager).close();
        Files.writeString(Path.of(args[0]),output.toString()+"\n");
    }
}
