import discord
from discord.ext import commands
from discord.ext.commands import has_permissions, MissingPermissions
from permissions import has_admin_role
from database import db, ServerConfigs

from util.logger import log

class Config(commands.Cog):
    def __init__(self, bot):
        self.bot = bot
    
    @commands.command(pass_context=True, aliases=["slc"])
    @commands.check(has_admin_role)
    async def setlogchannel(self, ctx, channel: discord.TextChannel):
        """Set the log channel for the server."""
        # change log channel locally
        ServerConfigs[ctx.guild.id]["log_channel_id"] = channel.id

        # change log channel remotely
        db.ServerConfigs.update_one({ "guildID": ctx.guild.id }, { "$set": { "log_channel_id": channel.id } })
        await ctx.send("Log channel set to {}.".format(channel.mention))
        await log(ctx, embed=discord.Embed(title="Log channel changed", description="Log channel set to {}.".format(channel.mention), color=discord.Color.green()))

def setup(bot):
    bot.add_cog(Config(bot))


