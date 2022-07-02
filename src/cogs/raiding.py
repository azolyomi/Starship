import discord
from permissions.checks import has_config
from util.converters import GlobalRaidType
from discord.ext import commands
from emoji import heart

class Raiding(commands.Cog):
    """Raiding cog for all things run-related."""
    def __init__(self, bot):
        self.bot = bot
    
    @commands.group(aliases=["run"])
    @commands.guild_only()
    async def raid(self, ctx):
        """Start or end a raid, via command."""
        if ctx.invoked_subcommand is None: 
            await ctx.send("Invalid usage. Do `{}help raid` for more information.".format(ctx.prefix))
    
    @raid.group(aliases=["create"], usage="<raid type>")
    @commands.check(has_config)
    async def start(self, ctx, raidType: GlobalRaidType):
        """Start a raid, via command."""
        if raidType == "vcless":
            await ctx.send("Vcless raids are not yet supported.")
        elif raidType is "vc":
            await ctx.send("Vc raids are not yet supported.")
        else:
            await ctx.send("Bruh.")

async def setup(bot):
    await bot.add_cog(Raiding(bot))