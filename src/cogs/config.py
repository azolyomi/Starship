import discord
from discord.ext import commands
from permissions.checks import has_admin_role, has_mod_role, is_urul
from database import ServerConfigs, updateConfig
from util.converters import GlobalRoleType
from emoji import check
from util import logger
from bson.json_util import dumps

ADDROLE_USAGE_STRING = "<roleType> <@role>"
SETLOGCHANNEL_USAGE_STRING = "<#channel>"

class Config(commands.Cog):
    def __init__(self, bot):
        self.bot = bot

    @commands.group(pass_context=True)
    @commands.check(has_mod_role)
    async def showconfig(self, ctx):
        """Show the current configuration for this server."""
        if (ctx.invoked_subcommand is None):
            # show config pretty embed
            pass
    
    @showconfig.command(pass_context=True)
    @commands.check(is_urul)
    async def debug(self, ctx): 
        config = dumps(ServerConfigs[ctx.guild.id], indent=4)
        configSubstr = config
        while len(configSubstr) > 0:
            await ctx.message.reply(embed=discord.Embed(title="Server Configuration", description="```{}```".format(configSubstr[0:2000]), color=discord.Color.purple()))
            configSubstr = configSubstr[2000:]
        
    @commands.command(pass_context=True, aliases=["slc"], usage="<#channel>")
    @commands.check(has_admin_role)
    async def setlogchannel(self, ctx, channel: discord.TextChannel):
        """Set the log channel for the server."""
        # change log channel locally
        ServerConfigs[ctx.guild.id]["log_channel_id"] = channel.id
        # change log channel remotely
        updateConfig(ctx.guild.id)

        await ctx.send("Log channel set to {}.".format(channel.mention))
        await logger.info(ctx, title="Log channel changed", description="Log channel set to {}.".format(channel.mention))

    @commands.group(pass_context=True, )
    @commands.check(has_mod_role)
    async def role(self, ctx):
        if ctx.invoked_subcommand is None:
                await ctx.message.reply('Invalid role command. Use `{}help role` for more information.'.format(ctx.prefix))
    
    
    @role.command(pass_context=True, aliases=["access"], usage="<roleType> <@role>")
    async def add(self, ctx, roleType: GlobalRoleType, *, role: discord.Role):
        """Add a role to the server's global configuration"""
        db_role_key = "{}roles".format(roleType)

        roleIDs = ServerConfigs[ctx.guild.id][db_role_key]
        if (role.id in roleIDs):
            await ctx.message.reply("{} is already in the `{}` role list.".format(role.mention, roleType))
            return

        roleIDs.append(role.id)
        # update locally
        ServerConfigs[ctx.guild.id][db_role_key] = roleIDs
        # update remotely
        updateConfig(ctx.guild.id)

        await ctx.message.add_reaction(check)
        await logger.info(ctx, title="Global role config updated", description="{0} was added to `{1}` roles.".format(role.mention, roleType))

    @role.command(pass_context=True, aliases=["unadd", "revoke"], usage="<roleType> <@role>")
    async def remove(self, ctx, roleType: GlobalRoleType, *, role: discord.Role):
        """Remove a role from the server's global configuration"""
        db_role_key = "{}roles".format(roleType)

        roleIDs = ServerConfigs[ctx.guild.id][db_role_key]
        if (role.id not in roleIDs):
            await ctx.message.reply("{} is not in the `{}` role list.".format(role.mention, roleType))
            return

        roleIDs.remove(role.id)
        # update locally
        ServerConfigs[ctx.guild.id][db_role_key] = roleIDs
        # update remotely
        updateConfig(ctx.guild.id)
        await ctx.message.add_reaction(check)
        await logger.info(ctx, title="Global role config updated", description="{0} was removed from `{1}` roles.".format(role.mention, roleType))

def setup(bot):
    bot.add_cog(Config(bot))


