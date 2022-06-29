import discord
import asyncio
from database import db, ServerConfigs
from emoji import a, b
from util.defaults import DEFAULT_VCLESS_CATEGORIES

async def create_vcless_channels_interactive(ctx):
        guild = ctx.guild
        category = await guild.create_category("The Hub")
        await category.edit(position=0)

        msg = await ctx.send(
            embed=discord.Embed(
                title="VC-Less Channel Preferences", 
                description="Would you like: \n:a: one text channel for each raid type, or \n:b: a single text channel where all vcless raids are created?", 
                color=discord.Color.blue()
            )
        )
        await msg.add_reaction(a)
        await msg.add_reaction(b)

        def check(reaction, user):
            return user == ctx.message.author and str(reaction.emoji) in [a, b]
        
        try:
            reaction, user = await ctx.bot.wait_for('reaction_add', timeout=30.0, check=check)
            if (str(reaction.emoji) == a):
                control_channel = await guild.create_text_channel("start-a-raid", category=category)
                await control_channel.edit(position=0)
                raiding_channel_ids = {}
                for (raidType) in DEFAULT_VCLESS_CATEGORIES:
                    text_channel = await guild.create_text_channel(raidType, category=category)
                    raiding_channel_ids[raidType] = text_channel.id
                return control_channel.id, raiding_channel_ids
            elif (str(reaction.emoji) == b):
                control_channel = await guild.create_text_channel("start-a-raid", category=category)
                await control_channel.edit(position=0)

                text_channel = await guild.create_text_channel("afk-checks", category=category)
                raiding_channel_ids = {}
                for (raidType) in DEFAULT_VCLESS_CATEGORIES:
                    raiding_channel_ids[raidType] = text_channel.id
                return control_channel.id, raiding_channel_ids
        except asyncio.TimeoutError:
            await ctx.send("Timed out. Please try again.")
            return None
