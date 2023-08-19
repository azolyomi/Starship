import discord
from util import logger
from util.constants import PREFIX
from discord.ext import commands
from database import db, ServerConfigs, updateConfig, deleteConfig
from permissions.checks import is_urul
from permissions.errors import StarshipConfigError

from emoji import starship

from util.setup.db import create_local_config

class Setup(commands.Cog):
    """A cog with one command for speedy setup of the bot."""
    def __init__(self, bot: commands.Bot):
        self.bot = bot

    @commands.command(pass_context=True, aliases=["reconfig"])
    @commands.check(is_urul)
    @commands.guild_only()
    async def reconfigure(self, ctx):
        if (ctx.guild.id in ServerConfigs):
            # not been cleared already
            for channel in ctx.guild.channels:
                if (channel.id == ctx.message.channel.id): continue
                await channel.delete()
            await ctx.send("All channels deleted.")
            for role in ctx.guild.roles:
                try:
                    await role.delete()
                except Exception as e:
                    print(e)
            await ctx.send("All roles deleted.")
            deleteConfig(ctx.guild.id)
            del ServerConfigs[ctx.guild.id]
        # been cleared already or never existed
        await self.configure(ctx)
    
    @commands.command(pass_context=True, aliases=['setup', 'config'])
    @commands.has_permissions(administrator=True)
    @commands.guild_only()
    async def configure(self, ctx):
        """Begin configuration process for the bot."""
        config = db.ServerConfigs.find_one({ "guildID": ctx.guild.id })
        if (config is not None):
            await ctx.send("This server has already been configured.")
            return

        await ctx.send("Configuring your server...")

        # create local config
        await ctx.send("Creating local configuration...")
        create_local_config(ctx.guild.id)

        # create administrative role
        await ctx.send("Creating administrative role...")
        admin_role = await ctx.guild.create_role(name="Starship Admin")
        ServerConfigs[ctx.guild.id]["adminroles"].append(admin_role.id)
        await admin_role.edit(color=discord.Color.orange())
        await ctx.message.author.add_roles(admin_role)
        await ctx.send(embed=discord.Embed(title="Warning:", description="The {} role will be added to the database as an `admin` role. All future commands with the Starship bot will be role-restricted.".format(admin_role.mention), color=discord.Color.red()))

        # create log channel
        await ctx.send("Creating log channel...")
        log_channel = await ctx.guild.create_text_channel("{}starship-log".format(starship))
        await log_channel.edit(position=0)
        ServerConfigs[ctx.guild.id]["log_channel_id"] = log_channel.id

        # update db config
        await ctx.send("Updating database entry...")
        updateConfig(ctx.guild.id)

        await ctx.send("Complete. You can now use the `{}help` command to see the list of commands.".format(PREFIX))
        


async def setup(bot):
    await bot.add_cog(Setup(bot))