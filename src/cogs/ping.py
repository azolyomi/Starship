import discord
from discord.ext import commands
from bot import bot

class Ping(commands.Cog):
    def __init__(self, bot):
        self.bot = bot

    @commands.command(aliases=["hello", "latency"])
    async def ping(self, ctx):
        """Test latency command"""
        await ctx.send(embed=discord.Embed(title="Pong!", description="Latency: `{} ms`".format(round(bot.latency * 1000)), color=discord.Color.darker_grey()))

def setup(bot):
    bot.add_cog(Ping(bot))