import discord
from database import ServerConfigs
from util.constants import THEURUL_USER_ID

def is_urul(ctx):
    return ctx.author.id == THEURUL_USER_ID

def has_config(ctx):
    return (ServerConfigs[ctx.guild.id] is not None)

def has_admin_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if role.id in ServerConfigs[ctx.guild.id]['adminroles']:
            return True
    return False

def has_mod_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if role.id in ServerConfigs[ctx.guild.id]['modroles']:
            return True
    return False

def has_staff_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if role.id in ServerConfigs[ctx.guild.id]['staffroles']:
            return True
    return False

def has_vcless_organizer_role(ctx, member, category):
    if not has_config(ctx): return False
    for role in member.roles:
        if role.id in ServerConfigs[ctx.guild.id]['raiding']['vcless']['categories'][category]['roles']:
            return True
    return False