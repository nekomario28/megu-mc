package dev.mcspine.tools;

import java.lang.reflect.*;
import java.util.*;
import java.util.function.Function;

/** Bind the actual pinned datapack tags; bootstrap registries alone leave them empty. */
public final class VanillaBlockTags {
    private VanillaBlockTags() {}
    static Class<?> cls(String n) throws Exception { return Class.forName(n); }
    @SuppressWarnings("unchecked")
    public static void load(Object registry) throws Exception {
        Object key=cls("jz").getMethod("d").invoke(registry);
        Object pack=cls("ats").getMethod("b").invoke(null);
        Object manager=cls("atx").getConstructor(cls("ass"),List.class)
            .newInstance(cls("ass").getField("b").get(null),List.of(pack));
        try {
            Method holder=cls("jz").getMethod("c",cls("akr"));
            Function<Object,Optional<?>> resolve=id->{
                try { return (Optional<?>)holder.invoke(registry,id); }
                catch(ReflectiveOperationException e) { throw new IllegalStateException(e); }
            };
            String directory=(String)cls("lu").getMethod("d",cls("akq")).invoke(null,key);
            Object loader=cls("awv").getConstructor(Function.class,String.class).newInstance(resolve,directory);
            Map<Object,Object> definitions=(Map<Object,Object>)cls("awv").getMethod("a",cls("aue")).invoke(loader,manager);
            Map<Object,Collection<?>> loaded=(Map<Object,Collection<?>>)cls("awv").getMethod("a",Map.class).invoke(loader,definitions);
            Set<Object> dropped=new HashSet<>(definitions.keySet());dropped.removeAll(loaded.keySet());
            if(!dropped.isEmpty()) throw new AssertionError("unresolved native block tags: "+dropped);
            if(loaded.isEmpty()) throw new AssertionError("no native block tags loaded");
            Map<Object,List<?>> bound=new HashMap<>();
            Method tag=cls("awu").getMethod("a",cls("akq"),cls("akr"));
            for(var e:loaded.entrySet()) bound.put(tag.invoke(null,key,e.getKey()),List.copyOf(e.getValue()));
            cls("jz").getMethod("n").invoke(registry);
            cls("jz").getMethod("a",Map.class).invoke(registry,bound);
            check(registry,key,"logs","oak_log",true);
            check(registry,key,"logs","stone",false);
            check(registry,key,"features_cannot_replace","bedrock",true);
            System.out.println("native block tags: "+loaded.size()+" loaded, 0 unresolved; logs/protection checked");
        } finally { ((AutoCloseable)manager).close(); }
    }
    static void check(Object registry,Object key,String tagName,String blockName,boolean expected) throws Exception {
        Object id=cls("akr").getMethod("a",String.class).invoke(null,"minecraft:"+blockName);
        Object block=cls("jz").getMethod("a",cls("akr")).invoke(registry,id);
        Object state=cls("dfy").getMethod("o").invoke(block);
        Object tid=cls("akr").getMethod("a",String.class).invoke(null,"minecraft:"+tagName);
        Object tag=cls("awu").getMethod("a",cls("akq"),cls("akr")).invoke(null,key,tid);
        boolean actual=(boolean)cls("dtc").getMethod("a",cls("awu")).invoke(state,tag);
        if(actual!=expected) throw new AssertionError("unbound/incorrect native tag "+tagName+" for "+blockName);
    }
}
