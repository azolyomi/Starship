import discord
import asyncio
from discord.ext import commands
from discord.ext.commands import has_permissions, MissingPermissions
from database import db

from emoji import starship

from util.setup.db_setup import initialize_db_serverconfig
from util.setup.vcless_setup import create_vcless_channels_interactive

from emoji import a, b

class Setup(commands.Cog):
    """A cog with one command for speedy setup of the bot."""
    def __init__(self, bot):
        self.bot = bot
    
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

        # create vcless channels
        await ctx.send("Creating vcless channels...")
        control_channel_id, raiding_channel_ids = await create_vcless_channels_interactive(ctx)
        await ctx.send("Vcless channels created.")

        # create administrative role
        await ctx.send("Creating administrative role...")
        admin_role = await ctx.guild.create_role(name="Starship Admin")
        await admin_role.edit(color=discord.Color.orange())
        await ctx.message.author.add_roles(admin_role)
        await ctx.send("Administrative role created.")
        await ctx.send(embed=discord.Embed(title="Warning:", description="The {} role will be added to the database as an \`admin\` role. All future commands with the Starship bot will be role-restricted.".format(admin_role.mention), color=discord.Color.red()))

        # create log channel
        await ctx.send("Creating log channel...")
        log_channel = await ctx.guild.create_text_channel("{}starship-log".format(starship))
        await log_channel.edit(position=0)
        await ctx.send("Log channel created.")

        # create default db entry
        await ctx.send("Initializing database entry...")
        await initialize_db_serverconfig(ctx, control_channel_id, raiding_channel_ids, admin_role_id = admin_role.id, log_channel_id = log_channel.id)
        await ctx.send("Database entry created.")


    @configure.error
    async def configure_error(self, ctx, error):
        if isinstance(error, MissingPermissions):
            await ctx.send("Sorry {}, you do not have permissions to do that!".format(ctx.message.author))
        else:
            print(error)

def setup(bot):
    bot.add_cog(Setup(bot))