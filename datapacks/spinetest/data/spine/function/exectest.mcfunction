scoreboard objectives add counter dummy
scoreboard players set @s counter 3
execute if score @s counter matches 1..5 run say counter in range!
execute positioned ~2 ~ ~1 run setblock ~ ~ ~ diamond_block
execute as @a at @s run summon pig ~ ~3 ~
say exectest done
