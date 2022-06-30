import discord
from discord.ext import commands
from discord.ext.commands import has_permissions
from permissions import has_admin_role, has_mod_role
from database import db, ServerConfigs
from util.constants import GLOBAL_ROLE_TYPES, PREFIX
from emoji import check

from util import logger

ADDROLE_USAGE_STRING = "`{}addrole <roleType> <@role>`".format(PREFIX)

class Config(commands.Cog):
    def __init__(self, bot):
        self.bot = bot
    
    @commands.command(pass_context=True, aliases=["slc"])
    @commands.guild_only()
    @commands.check(has_admin_role)
    async def setlogchannel(self, ctx, channel: discord.TextChannel):
        """Set the log channel for the server."""
        # change log channel remotely
        db.ServerConfigs.update_one({ "guildID": ctx.guild.id }, { "$set": { "log_channel_id": channel.id } })

        # change log channel locally
        ServerConfigs[ctx.guild.id]["log_channel_id"] = channel.id

        await ctx.send("Log channel set to {}.".format(channel.mention))
        await logger.info(ctx, title="Log channel changed", description="Log channel set to {}.".format(channel.mention))

    class GlobalRoleType(commands.Converter):
        async def convert(self, ctx, argument):
            if (argument in GLOBAL_ROLE_TYPES): return argument
            raise commands.BadArgument("`roleType` must be one of `{}`".format(GLOBAL_ROLE_TYPES))
    
    @commands.command(pass_context=True, aliases=["accessrole", "ar"])
    @commands.guild_only()
    @commands.check(has_mod_role)
    async def addrole(self, ctx, roleType: GlobalRoleType, role: discord.Role):
        """Add a role to the server's `global` configuration. If you're looking to add `organizer` roles to a category, try `.help addorganizer`"""
        db_role_key = "{}roles".format(roleType)

        roleIDs = ServerConfigs[ctx.guild.id][db_role_key]
        if (role.id in roleIDs):
            await ctx.message.reply("{} is already in the `{}` role list.".format(role.mention, roleType))
            return

        roleIDs.append(role.id)
        # update db remotely
        db.ServerConfigs.update_one({ "guildID": ctx.guild.id }, { "$set": { db_role_key : roleIDs } })
        # update locally
        ServerConfigs[ctx.guild.id][db_role_key] = roleIDs

        await ctx.message.add_reaction(check)
        await logger.info(ctx, title="Global role config updated", description="{0} was added to `{1}` roles.".format(role.mention, roleType))

    @addrole.error
    async def addrole_error(self, ctx, error):
        if isinstance(error, commands.MissingRequiredArgument):
            await ctx.message.reply("You're missing a required argument. Usage: {}".format(ADDROLE_USAGE_STRING))
        elif isinstance(error, commands.BadArgument):
            await ctx.message.reply(error)
        elif isinstance(error, commands.CheckFailure):
            await ctx.message.reply("You don't have permission to use this command.")
        else:
            await ctx.message.reply("An unknown error occurred.")
            await logger.err(ctx, title="Unknown error", description="{}".format(error))

def setup(bot):
    bot.add_cog(Config(bot))


