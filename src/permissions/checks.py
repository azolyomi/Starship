from discord.ext.commands import NoPrivateMessage
from database import ServerConfigs
from util.constants import THEURUL_USER_ID

from permissions import errors

def is_urul(ctx):
    return ctx.author.id == THEURUL_USER_ID

# LOCAL UTIL FUNCS

def is_admin(ctx, roleid): 
    return roleid in ServerConfigs[ctx.guild.id]['adminroles']

def is_mod(ctx, roleid):
    return roleid in ServerConfigs[ctx.guild.id]['modroles']

def is_staff(ctx, roleid):
    return roleid in ServerConfigs[ctx.guild.id]['staffroles']

# EXPORTED FUNCS

# equivalent to guild_only() + check config exists
def has_config(ctx):
    if (ctx.guild is None): 
        raise NoPrivateMessage()
    return (ServerConfigs[ctx.guild.id] is not None)
    
# equivalent to guild_only() + check config exists + check admin role in config
def has_admin_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if is_admin(ctx, role.id):
            return True
    raise errors.StarshipRoleMissingError("admin")

def has_mod_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if is_mod(ctx, role.id) or is_admin(ctx, role.id):
            return True
    raise errors.StarshipRoleMissingError("mod")

def has_staff_role(ctx):
    if not has_config(ctx): return False
    for role in ctx.author.roles:
        if is_staff(ctx, role.id) or is_admin(ctx, role.id):
            return True
    raise errors.StarshipRoleMissingError("staff")

def has_vcless_organizer_role(ctx, member, category):
    if not has_config(ctx): return False
    elif category not in ServerConfigs[ctx.guild.id]["raiding"]["vcless"]["categories"]:
        raise errors.StarshipCategoryNotFoundError(category)
    for role in member.roles:
        if role.id in ServerConfigs[ctx.guild.id]['raiding']['vcless']['categories'][category]['organizer_roles']:
            return True
    raise errors.StarshipRoleMissingError("{} organizer".format(category))