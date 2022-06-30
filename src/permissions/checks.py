from discord.ext.commands import NoPrivateMessage
from database import ServerConfigs
from util.constants import THEURUL_USER_ID

from permissions import errors

def is_urul(ctx):
    return ctx.author.id == THEURUL_USER_ID

def has_config(ctx):
    if (ctx.guild is None): 
        raise NoPrivateMessage()
    return (ServerConfigs[ctx.guild.id] is not None)
    

def has_admin_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if role.id in ServerConfigs[ctx.guild.id]['adminroles']:
            return True
    raise errors.NotAdminError()

def has_mod_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if role.id in ServerConfigs[ctx.guild.id]['modroles']:
            return True
    raise errors.NotModError()

def has_staff_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if role.id in ServerConfigs[ctx.guild.id]['staffroles']:
            return True
    raise errors.NotStaffError()

# def has_vcless_organizer_role(ctx, member, category):
#     if not has_config(ctx): return False
#     for role in member.roles:
#         if role.id in ServerConfigs[ctx.guild.id]['raiding']['vcless']['categories'][category]['roles']:
#             return True
#     return False