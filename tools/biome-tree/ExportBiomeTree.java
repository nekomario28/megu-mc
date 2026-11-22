// Do not put the transport in the default package: the obfuscated game classes
// occupy that package in a signed jar, so an unsigned peer triggers a signer error.
package dev.megu.tools;

import com.google.gson.JsonArray;
import com.google.gson.JsonObject;
import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;

/** Original transport for the pinned 1.21.1 classes, not a reimplementation of their builders.
 * Inspired by Horizon's VanillaBiomeExport (MIT, Copyright (c) 2026 Ludovic).
 * See third_party/licenses/horizon.txt and evidence/foundation/horizon-reuse/README.md.
 * All obfuscated names below are resolved in the official 1.21.1 server mappings.
 */
public final class ExportBiomeTree {
    private static Object field(Object value, String owner, String name) throws Exception {
        Field f = Class.forName(owner).getDeclaredField(name);
        f.setAccessible(true);
        return f.get(value);
    }

    private static JsonObject tree(Object node) throws Exception {
        JsonObject out = new JsonObject();
        // Climate$RTree$Node.parameterSpace -> def$e$b.a
        Object space = field(node, "def$e$b", "a");
        if (Array.getLength(space) != 7) throw new IllegalStateException("not seven climate axes");
        JsonArray parameters = new JsonArray();
        for (int i = 0; i < 7; ++i) {
            Object parameter = Array.get(space, i);
            JsonObject interval = new JsonObject();
            // Climate$Parameter.min()/max() -> def$b.a()/b()
            interval.addProperty("min", (Long) parameter.getClass().getMethod("a").invoke(parameter));
            interval.addProperty("max", (Long) parameter.getClass().getMethod("b").invoke(parameter));
            parameters.add(interval);
        }
        out.add("parameters", parameters);
        if (node.getClass().getName().equals("def$e$a")) {
            out.addProperty("_type", "leaf");
            // Climate$RTree$Leaf.value -> b; ResourceKey.location() -> akq.a()
            Object key = field(node, "def$e$a", "b");
            out.addProperty("biome", key.getClass().getMethod("a").invoke(key).toString());
        } else if (node.getClass().getName().equals("def$e$c")) {
            out.addProperty("_type", "branch");
            Object children = field(node, "def$e$c", "b");
            JsonArray subTree = new JsonArray();
            for (int i = 0; i < Array.getLength(children); ++i) subTree.add(tree(Array.get(children, i)));
            out.add("subTree", subTree);
        } else {
            throw new IllegalStateException("unexpected climate node " + node.getClass());
        }
        return out;
    }

    public static void main(String[] args) throws Exception {
        // SharedConstants.tryDetectVersion / Bootstrap.bootStrap
        Class.forName("ab").getMethod("a").invoke(null);
        Class.forName("akt").getMethod("a").invoke(null);
        // MultiNoiseBiomeSourceParameterList.knownPresets() -> dek.b()
        Map<?, ?> presets = (Map<?, ?>) Class.forName("dek").getMethod("b").invoke(null);
        JsonObject out = new JsonObject();
        for (Map.Entry<?, ?> entry : presets.entrySet()) {
            // Preset.id() -> dek$a.b(); ParameterList.index -> def$c.b; RTree.root -> def$e.b
            String id = entry.getKey().getClass().getMethod("b").invoke(entry.getKey()).toString();
            Object index = field(entry.getValue(), "def$c", "b");
            out.add(id.substring("minecraft:".length()), tree(field(index, "def$e", "b")));
        }
        Files.writeString(Path.of(args[0]), out.toString());
    }
}
