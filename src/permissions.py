import discord
from database import ServerConfigs

def has_config_admin_role(ctx):
    config = ServerConfigs[ctx.guild.id]
    if (config is None):
        return False
    for role in ctx.author.roles:
        if role.id in config['adminroles']:
            return True
    return False