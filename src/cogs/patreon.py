import discord
from discord.ext import commands
from emoji import heart

class Patreon(commands.Cog):
    """Patreon cog to link users to buy me a coffee."""
    def __init__(self, bot):
        self.bot = bot
    
    @commands.command(aliases=["owner", "coffee", "support", "pog", "pogchamp"])
    async def patreon(self, ctx):
        """Link to the patreon page and to buy me a coffee."""
        embed = discord.Embed(
            title="Support the Creator",
            description="If you want to **support me**, please consider [buying me a coffee](https://www.buymeacoffee.com/Theurul)\n\nYou can also [subscribe to my patreon](https://patreon.com/theurul), where supporters are given access to **cool bot features** and unique server permissions in various communities. ",
            color=discord.Color.orange()
        )
        embed.set_thumbnail(url="https://cdn.discordapp.com/avatars/942320785287184464/eac00b47a7f6b7883d1260f8bb3111e6.png?size=4096")
        embed.set_footer(text="Your local, friendly bot dev Theurul {}".format(heart))
        await ctx.send(embed=embed)


async def setup(bot):
    await bot.add_cog(Patreon(bot))