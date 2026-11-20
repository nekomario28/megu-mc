package net.minecraft.world.entity.player;

import net.mcspine.neoforge.MutationBuffer;
import net.minecraft.network.chat.Component;
import net.minecraft.world.entity.LivingEntity;

public class Player extends LivingEntity {
    private final int eid;
    private final Component name;

    public Player(int eid, String name) {
        this.eid = eid;
        this.name = Component.literal(name);
    }

    public int getEid() { return eid; }
    public Component getName() { return name; }
    public void sendSystemMessage(Component message) {
        MutationBuffer.append(new MutationBuffer.SendSystemMessage(eid, message.getString()));
    }
}
