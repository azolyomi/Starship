import discord
from discord.ext import commands

class Test(commands.Cog):
    def __init__(self, bot):
        self.bot = bot

    @commands.command()
    async def hello(self, ctx):
        """Test command"""
        await ctx.send(embed=discord.Embed(title="me", description="your mom", color=0xff0000))

def setup(bot):
    bot.add_cog(Test(bot))