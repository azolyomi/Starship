import discord
from discord.ext import commands

class Ping(commands.Cog):
    """A cog with tools to evaluate bot latency."""
    def __init__(self, bot):
        self.bot = bot

    @commands.command(aliases=["hello", "latency"])
    async def ping(self, ctx):
        """Test latency command"""
        await ctx.send(embed=discord.Embed(title="Pong!", description="Latency: `{} ms`".format(round(self.bot.latency * 1000)), color=discord.Color.darker_grey()))

async def setup(bot):
    await bot.add_cog(Ping(bot))