import discord
from database import ServerConfigs

def is_urul(ctx):
    return ctx.author.id == 942320785287184464

def has_config(ctx):
    return (ServerConfigs[ctx.guild.id] is not None)

def has_admin_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if role.id in ServerConfigs[ctx.guild.id]['adminroles']:
            return True
    return False